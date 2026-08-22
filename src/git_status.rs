// Git status probe via gix (gitoxide): branch, dirty flag, ahead/behind and
// the +N/-N line counts read straight from the repository database and the
// worktree — no `git` binary to resolve on PATH or spawn. Results are
// memoized across render processes for 15 seconds per working copy (the key
// is a SHA-256 of the canonical cwd, never the path itself).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gix::bstr::{BString, ByteSlice};
use gix::status::index_worktree;
use serde::{Deserialize, Serialize};

use crate::util;

const GIT_STATUS_TTL_MS: u64 = 15_000;
const GIT_STATUS_CACHE_MAX_ENTRIES: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GitEntry {
    branch: Option<String>,
    dirty: bool,
    #[serde(default)]
    ahead: u32,
    #[serde(default)]
    behind: u32,
    #[serde(default)]
    diff_added: u32,
    #[serde(default)]
    diff_deleted: u32,
    checked_at: u64,
}

/// One probe's outcome, the fields the footer git badge needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitSummary {
    pub branch: Option<String>,
    pub dirty: bool,
    pub ahead: u32,
    pub behind: u32,
    pub diff_added: u32,
    pub diff_deleted: u32,
}

fn cache_key(cwd: &str) -> String {
    let normalized = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    util::sha256_hex(normalized.to_string_lossy().as_bytes())
}

fn read_cache(cache_path: &Path) -> GitCache {
    util::read_string(cache_path)
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(|cache: &GitCache| cache.v == 1)
        .unwrap_or_default()
}

fn write_cache(cache_path: &Path, mut cache: GitCache) {
    if cache.entries.len() > GIT_STATUS_CACHE_MAX_ENTRIES {
        let mut keys: Vec<String> = cache.entries.keys().cloned().collect();
        keys.sort_by_key(|k| cache.entries[k].checked_at);
        let drop = cache.entries.len() - GIT_STATUS_CACHE_MAX_ENTRIES;
        for key in keys.into_iter().take(drop) {
            cache.entries.remove(&key);
        }
    }
    if let Ok(text) = serde_json::to_string(&cache) {
        let _ = util::atomic_write(cache_path, text.as_bytes());
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GitCache {
    v: u32,
    #[serde(default)]
    entries: HashMap<String, GitEntry>,
}

/// The working copy's status for the footer badge. Uses the cross-process
/// cache when fresh; the gix probe is fail-open — an unreadable repository
/// yields a clean summary rather than blocking or crashing a frame.
pub fn git_status(cwd: &str, cache_path: &Path) -> GitSummary {
    if cwd.is_empty() {
        return GitSummary::default();
    }
    let now = util::now_ms();
    let key = cache_key(cwd);
    let cache = read_cache(cache_path);
    if let Some(entry) = cache.entries.get(&key) {
        if now.saturating_sub(entry.checked_at) < GIT_STATUS_TTL_MS {
            return entry_summary(entry);
        }
    }
    let summary = probe(cwd);
    let mut next = cache;
    next.v = 1;
    next.entries.insert(
        key,
        GitEntry {
            branch: summary.branch.clone(),
            dirty: summary.dirty,
            ahead: summary.ahead,
            behind: summary.behind,
            diff_added: summary.diff_added,
            diff_deleted: summary.diff_deleted,
            checked_at: now,
        },
    );
    write_cache(cache_path, next);
    summary
}

fn entry_summary(entry: &GitEntry) -> GitSummary {
    GitSummary {
        branch: entry.branch.clone(),
        dirty: entry.dirty,
        ahead: entry.ahead,
        behind: entry.behind,
        diff_added: entry.diff_added,
        diff_deleted: entry.diff_deleted,
    }
}

fn probe(cwd: &str) -> GitSummary {
    let repo = match gix::discover(cwd) {
        Ok(repo) => repo,
        Err(_) => return GitSummary::default(),
    };
    let branch = repo
        .head_name()
        .ok()
        .flatten()
        .map(|name| name.shorten().to_string());
    let (dirty, mut paths) = changed_paths(&repo);
    let mut summary = GitSummary {
        branch,
        dirty,
        ..GitSummary::default()
    };
    if dirty {
        paths.sort();
        paths.dedup();
        let (added, deleted) = numstat(&repo, &paths);
        summary.diff_added = added;
        summary.diff_deleted = deleted;
    }
    if let Some((ahead, behind)) = ahead_behind(&repo) {
        summary.ahead = ahead;
        summary.behind = behind;
    }
    summary
}

/// One status pass: any tracked change or untracked file marks dirty (like
/// `git status --porcelain`, ignored files never appear); the paths of the
/// tracked changes (staged and unstaged) are the numstat candidates.
fn changed_paths(repo: &gix::Repository) -> (bool, Vec<BString>) {
    use gix::status::Item;

    let Ok(platform) = repo.status(gix::progress::Discard) else {
        return (false, Vec::new());
    };
    let Ok(iter) = platform.into_iter(None) else {
        return (false, Vec::new());
    };
    let mut dirty = false;
    let mut paths: Vec<BString> = Vec::new();
    for item in iter.flatten() {
        match item {
            Item::TreeIndex(change) => {
                dirty = true;
                paths.push(change.location().to_owned());
            }
            Item::IndexWorktree(item) => match item {
                index_worktree::Item::Modification { rela_path, .. } => {
                    dirty = true;
                    paths.push(rela_path);
                }
                index_worktree::Item::DirectoryContents { entry, .. } => {
                    if matches!(entry.status, gix::dir::entry::Status::Untracked) {
                        dirty = true;
                    }
                }
                index_worktree::Item::Rewrite { source, dirwalk_entry, .. } => {
                    dirty = true;
                    paths.push(source.rela_path().to_owned());
                    paths.push(dirwalk_entry.rela_path.to_owned());
                }
            },
        }
    }
    (dirty, paths)
}

/// The `git diff --numstat HEAD` equivalent for the changed paths: per path,
/// a line diff of the HEAD blob against the filtered (CRLF etc.) worktree
/// content. A path missing from the worktree falls back to counting the HEAD
/// blob's lines. Binary content and external diff drivers count as 0, like
/// numstat's "-".
fn numstat(repo: &gix::Repository, paths: &[BString]) -> (u32, u32) {
    use gix::diff::blob::pipeline::{Mode, WorktreeRoots};
    use gix::diff::blob::ResourceKind;
    use gix::objs::tree::EntryKind;
    use gix::object::blob::diff::{lines, Platform};

    let Some(workdir) = repo.workdir() else {
        return (0, 0);
    };
    let Ok(mut cache) = repo.diff_resource_cache(
        Mode::ToGit,
        WorktreeRoots {
            old_root: None,
            new_root: Some(workdir.to_path_buf()),
        },
    ) else {
        return (0, 0);
    };
    let head_tree = repo.head_tree().ok();
    let null_id = gix::hash::ObjectId::null(repo.object_hash());
    let mut added = 0u32;
    let mut deleted = 0u32;
    for path in paths {
        let head_entry = head_tree.as_ref().and_then(|tree| {
            tree.lookup_entry(path.split_str("/"))
                .ok()
                .flatten()
        });
        let worktree_path = match path.to_path() {
            Ok(relative) => workdir.join(relative),
            Err(_) => continue,
        };
        let present = std::fs::symlink_metadata(&worktree_path).is_ok();
        if present {
            let old_id = head_entry
                .as_ref()
                .map(|entry| entry.id().detach())
                .unwrap_or(null_id);
            let kind = match head_entry.as_ref().map(|entry| entry.mode()) {
                Some(mode) if mode.is_link() => EntryKind::Link,
                _ => EntryKind::Blob,
            };
            if cache
                .set_resource(old_id, kind, path.as_bstr(), ResourceKind::OldOrSource, &repo.objects)
                .is_err()
            {
                continue;
            }
            if cache
                .set_resource(null_id, kind, path.as_bstr(), ResourceKind::NewOrDestination, &repo.objects)
                .is_err()
            {
                continue;
            }
            let mut platform = Platform { resource_cache: &mut cache };
            let _ = platform.lines(|change| -> Result<(), std::convert::Infallible> {
                match change {
                    lines::Change::Addition { lines } => added += lines.len() as u32,
                    lines::Change::Deletion { lines } => deleted += lines.len() as u32,
                    lines::Change::Modification { lines_before, lines_after } => {
                        added += lines_after.len() as u32;
                        deleted += lines_before.len() as u32;
                    }
                }
                Ok(())
            });
        } else if let Some(entry) = head_entry {
            deleted += blob_line_count(repo, entry.id().detach());
        }
    }
    (added, deleted)
}

/// Line count of one blob, 0 for binary content (git's NUL-in-head heuristic).
fn blob_line_count(repo: &gix::Repository, id: gix::hash::ObjectId) -> u32 {
    let Ok(object) = repo.find_object(id) else {
        return 0;
    };
    let Ok(blob) = object.try_into_blob() else {
        return 0;
    };
    let head = &blob.data[..blob.data.len().min(8000)];
    if head.contains(&0u8) {
        return 0;
    }
    blob.data.lines().count() as u32
}

/// Local ahead/behind against the configured upstream, mirroring
/// `git status`'s `[ahead N, behind M]` (remote-tracking refs only, no
/// network). The default-fetch-refspec mapping covers the standard
/// `refs/remotes/<remote>/<branch>` shape.
fn ahead_behind(repo: &gix::Repository) -> Option<(u32, u32)> {
    let branch = repo.head_name().ok().flatten()?.shorten().to_string();
    let remote = repo
        .config_snapshot()
        .string(format!("branch.{branch}.remote").as_str())?
        .to_string();
    if remote == "." {
        return None;
    }
    let merge = repo
        .config_snapshot()
        .string(format!("branch.{branch}.merge").as_str())?
        .to_string();
    let short = merge.strip_prefix("refs/heads/")?;
    let upstream_ref = format!("refs/remotes/{remote}/{short}");
    let upstream_id = repo
        .find_reference(&upstream_ref)
        .ok()?
        .into_fully_peeled_id()
        .ok()?
        .detach();
    let head_id = repo.head_id().ok()?.detach();
    let ahead = count_reachable(repo, head_id, upstream_id);
    let behind = count_reachable(repo, upstream_id, head_id);
    Some((ahead, behind))
}

/// Commits reachable from `from` but not from `hide`.
fn count_reachable(repo: &gix::Repository, from: gix::hash::ObjectId, hide: gix::hash::ObjectId) -> u32 {
    repo.rev_walk(Some(from))
        .sorting(gix::revision::walk::Sorting::BreadthFirst)
        .with_hidden(Some(hide))
        .all()
        .ok()
        .map(|walk| walk.filter_map(Result::ok).count() as u32)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::objs::tree::EntryKind;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kimi-hud-rs-git-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn signature() -> gix::actor::SignatureRef<'static> {
        gix::actor::SignatureRef {
            name: "hud".into(),
            email: "hud@example.com".into(),
            time: "0 +0000",
        }
    }

    /// Commit `files` on top of `parents`, sync the index to the new tree and
    /// return the commit id.
    fn commit(repo: &gix::Repository, files: &[(&str, &str)], parents: &[gix::hash::ObjectId]) -> gix::hash::ObjectId {
        let mut editor = match parents
            .first()
            .and_then(|id| repo.find_commit(*id).ok())
            .and_then(|commit| commit.tree_id().ok())
        {
            Some(tree_id) => repo.edit_tree(tree_id).unwrap(),
            None => repo.empty_tree().edit().unwrap(),
        };
        for (path, content) in files {
            let blob = repo.write_blob(content).unwrap();
            editor.upsert(*path, EntryKind::Blob, blob).unwrap();
        }
        let tree = editor.write().unwrap();
        let sig = signature();
        let commit_id = repo
            .commit_as(sig, sig, "refs/heads/main", "test", tree, parents.iter().copied())
            .unwrap()
            .detach();
        let mut index = repo.index_from_tree(&tree).unwrap();
        index.write(gix::index::write::Options::default()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        commit_id
    }

    fn write_file(root: &Path, path: &str, content: &str) {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }

    #[test]
    fn probe_is_silent_on_non_repo() {
        let dir = temp_dir("nonrepo");
        let summary = git_status(&dir.to_string_lossy(), &dir.join("git.json"));
        assert_eq!(summary, GitSummary::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_repo_reports_branch_and_no_changes() {
        let dir = temp_dir("clean");
        write_file(&dir, "a.txt", "one\ntwo\n");
        let repo = gix::init(&dir).unwrap();
        commit(&repo, &[("a.txt", "one\ntwo\n")], &[]);
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let summary = probe(&dir.to_string_lossy());
        assert_eq!(summary.branch.as_deref(), Some("main"));
        assert!(!summary.dirty);
        assert_eq!((summary.diff_added, summary.diff_deleted), (0, 0));
        assert_eq!((summary.ahead, summary.behind), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn modification_counts_lines_against_head() {
        let dir = temp_dir("modify");
        write_file(&dir, "a.txt", "one\ntwo\n");
        let repo = gix::init(&dir).unwrap();
        commit(&repo, &[("a.txt", "one\ntwo\n")], &[]);
        // Unstaged edit: one line replaced, one added.
        write_file(&dir, "a.txt", "one\nTWO\nthree\n");
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let summary = probe(&dir.to_string_lossy());
        assert!(summary.dirty);
        assert_eq!(summary.diff_added, 2);
        assert_eq!(summary.diff_deleted, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn untracked_marks_dirty_without_counts() {
        let dir = temp_dir("untracked");
        write_file(&dir, "a.txt", "one\n");
        let repo = gix::init(&dir).unwrap();
        commit(&repo, &[("a.txt", "one\n")], &[]);
        write_file(&dir, "new.txt", "fresh\n");
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let summary = probe(&dir.to_string_lossy());
        assert!(summary.dirty);
        assert_eq!((summary.diff_added, summary.diff_deleted), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_deletion_counts_head_lines() {
        let dir = temp_dir("delete");
        write_file(&dir, "a.txt", "one\ntwo\nthree\n");
        let repo = gix::init(&dir).unwrap();
        commit(&repo, &[("a.txt", "one\ntwo\nthree\n")], &[]);
        std::fs::remove_file(dir.join("a.txt")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let summary = probe(&dir.to_string_lossy());
        assert!(summary.dirty);
        assert_eq!(summary.diff_deleted, 3);
        assert_eq!(summary.diff_added, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ahead_behind_against_remote_tracking_ref() {
        let dir = temp_dir("ahead");
        write_file(&dir, "a.txt", "one\n");
        let repo = gix::init(&dir).unwrap();
        let first = commit(&repo, &[("a.txt", "one\n")], &[]);
        // Origin tracks the first commit; local main moves one ahead.
        repo.reference(
            "refs/remotes/origin/main",
            first,
            gix::refs::transaction::PreviousValue::Any,
            "test",
        )
        .unwrap();
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(".git").join("config"))
            .unwrap();
        use std::io::Write;
        writeln!(
            config,
            "[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n"
        )
        .unwrap();
        drop(config);
        write_file(&dir, "a.txt", "one\ntwo\n");
        commit(&repo, &[("a.txt", "one\ntwo\n")], &[first]);
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let summary = probe(&dir.to_string_lossy());
        assert_eq!((summary.ahead, summary.behind), (1, 0));
        // The second commit staged+synced the index, but the worktree matches,
        // so the tree is clean.
        assert!(!summary.dirty);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

