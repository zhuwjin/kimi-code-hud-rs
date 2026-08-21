// Git dirty check: a PATH-resolved `git status --porcelain` probe with a hard
// timeout, memoized across render processes for 15 seconds per working copy
// (the key is a SHA-256 of the canonical cwd, never the path itself).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::util;

const GIT_STATUS_TTL_MS: u64 = 15_000;
const GIT_STATUS_CACHE_MAX_ENTRIES: usize = 64;
const DEFAULT_WIN32_PATHEXT: [&str; 4] = [".COM", ".EXE", ".BAT", ".CMD"];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GitEntry {
    branch: Option<String>,
    dirty: bool,
    checked_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GitCache {
    v: u32,
    #[serde(default)]
    entries: HashMap<String, GitEntry>,
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn path_extensions() -> Vec<String> {
    if !cfg!(windows) {
        return vec![String::new()];
    }
    let raw = std::env::var("PATHEXT").unwrap_or_default();
    if raw.trim().is_empty() {
        return DEFAULT_WIN32_PATHEXT.iter().map(|s| s.to_string()).collect();
    }
    raw.split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn candidate_names(command: &str, extensions: &[String]) -> Vec<String> {
    if extensions.len() == 1 && extensions[0].is_empty() {
        return vec![command.to_string()];
    }
    let lower = command.to_lowercase();
    if extensions.iter().any(|ext| lower.ends_with(&ext.to_lowercase())) {
        let mut names = vec![command.to_string()];
        names.extend(extensions.iter().map(|ext| format!("{}{}", command, ext)));
        return names;
    }
    extensions.iter().map(|ext| format!("{}{}", command, ext)).collect()
}

/// Resolve a bare command to an absolute executable from PATH. A hit inside
/// cwd fails closed (cmd.exe/CreateProcess search the working directory
/// first) instead of falling through to a later PATH entry.
fn resolve_command_path(command: &str, cwd: &str) -> Option<PathBuf> {
    if command.is_empty() || command.contains('/') || command.contains('\\') {
        return None;
    }
    let cwd_real = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in candidate_names(command, &path_extensions()) {
            let candidate = dir.join(&name);
            if !is_executable(&candidate) {
                continue;
            }
            let real = std::fs::canonicalize(&candidate).ok()?;
            let inside_cwd = {
                #[cfg(windows)]
                {
                    let a = real.to_string_lossy().to_lowercase();
                    let b = cwd_real.to_string_lossy().to_lowercase();
                    a.starts_with(&b)
                }
                #[cfg(not(windows))]
                {
                    real.starts_with(&cwd_real)
                }
            };
            if inside_cwd {
                return None;
            }
            return Some(real);
        }
    }
    None
}

fn parse_branch(summary: &str) -> Option<String> {
    if let Some(branch) = summary.strip_prefix("No commits yet on ") {
        return (!branch.is_empty()).then(|| branch.to_string());
    }
    if let Some(branch) = summary.strip_prefix("Initial commit on ") {
        return (!branch.is_empty()).then(|| branch.to_string());
    }
    if summary == "HEAD" || summary.starts_with("HEAD ") {
        return None;
    }
    let upstream = summary.find("...");
    let tracking = summary.find(" [");
    let end = upstream.unwrap_or(summary.len()).min(tracking.unwrap_or(summary.len()));
    let branch = &summary[..end];
    (!branch.is_empty()).then(|| branch.to_string())
}

fn parse_git_status(output: &str) -> (Option<String>, bool) {
    let mut branch = None;
    let mut dirty = false;
    for line in output.split(|c| c == '\n' || c == '\r') {
        let line = line.trim_end_matches('\r');
        if let Some(summary) = line.strip_prefix("## ") {
            branch = parse_branch(summary);
        } else if !line.is_empty() {
            dirty = true;
        }
    }
    (branch, dirty)
}

fn spawn_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let mut file = stdout;
        let _ = file.read_to_string(&mut buf);
        buf
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = reader.join().unwrap_or_default();
                return if status.success() { Some(output) } else { None };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
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

/// Whether the working copy has uncommitted changes. Uses the cross-process
/// cache when fresh, and on any failure falls back to the last cached value
/// (or clean) — the dirty marker must never block or crash a frame.
pub fn is_git_dirty(cwd: &str, timeout: Duration, cache_path: &Path) -> bool {
    if cwd.is_empty() {
        return false;
    }
    let now = util::now_ms();
    let key = cache_key(cwd);
    let cache = read_cache(cache_path);
    let cached = cache.entries.get(&key).cloned();
    if let Some(entry) = &cached {
        if now.saturating_sub(entry.checked_at) < GIT_STATUS_TTL_MS {
            return entry.dirty;
        }
    }
    let fallback = cached.as_ref().map(|e| e.dirty).unwrap_or(false);
    let Some(git) = resolve_command_path("git", cwd) else {
        return fallback;
    };
    let mut cmd = Command::new(&git);
    cmd.args(["status", "--porcelain=v1", "--branch"])
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0");
    let Some(output) = spawn_with_timeout(&mut cmd, timeout) else {
        return fallback;
    };
    let (branch, dirty) = parse_git_status(&output);
    let mut next = cache;
    next.v = 1;
    next.entries.insert(
        key,
        GitEntry {
            branch,
            dirty,
            checked_at: now,
        },
    );
    write_cache(cache_path, next);
    dirty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_branch_and_dirty() {
        let (branch, dirty) = parse_git_status("## main...origin/main [ahead 1]\nM src/a.rs\n?? b\n");
        assert_eq!(branch.as_deref(), Some("main"));
        assert!(dirty);

        let (branch, dirty) = parse_git_status("## main\n");
        assert_eq!(branch.as_deref(), Some("main"));
        assert!(!dirty);

        let (branch, _) = parse_git_status("## HEAD (no branch)\n");
        assert_eq!(branch, None);

        let (branch, _) = parse_git_status("## No commits yet on master\n");
        assert_eq!(branch.as_deref(), Some("master"));
    }

    #[test]
    fn dirty_check_is_silent_on_non_repo() {
        let dir = std::env::temp_dir().join(format!("kimi-hud-rs-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache_path = dir.join("git.json");
        // Not a git repository: git exits non-zero, the probe falls back to
        // "clean" without panicking and without blocking on the timeout.
        let dirty = is_git_dirty(&dir.to_string_lossy(), Duration::from_millis(200), &cache_path);
        assert!(!dirty);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
