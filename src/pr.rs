// PR badge data: `gh pr view` for the working copy's branch, cached per
// (cwd, branch) for 60s behind the same detached-refresh pattern as the
// quota cache — the render hot path never spawns or blocks. Mirrors the
// host footer's readPullRequest, including gh PATH resolution that fails
// closed on a hit inside the working copy and strict validation of the
// helper's JSON (the URL ends up inside an OSC 8 hyperlink).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::quota::{acquire_quota_lock, release_quota_lock};
use crate::util;

pub const PR_TTL_MS: u64 = 60_000;
const GH_SPAWN_TIMEOUT_MS: u64 = 5_000;
const PR_CACHE_MAX_ENTRIES: usize = 16;
const DEFAULT_WIN32_PATHEXT: [&str; 4] = [".COM", ".EXE", ".BAT", ".CMD"];

/// GitHub PR states: OPEN renders in primary, MERGED/CLOSED in the muted
/// purple the user picked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrInfo {
    pub number: u64,
    pub url: String,
    #[serde(default = "open_state")]
    pub state: String,
}

fn open_state() -> String {
    "OPEN".to_string()
}

/// One cached lookup outcome; number 0 encodes a known absence (no PR, no
/// gh, no auth) so failures throttle exactly like successes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrEntry {
    number: u64,
    #[serde(default)]
    url: String,
    #[serde(default = "open_state")]
    state: String,
    fetched_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PrCache {
    v: u32,
    #[serde(default)]
    entries: HashMap<String, PrEntry>,
}

pub struct PrLookup {
    pub info: Option<PrInfo>,
    /// True when the entry is missing or past its TTL — kick a refresh.
    pub stale: bool,
}

fn cache_key(cwd: &str, branch: &str) -> String {
    let normalized = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    util::sha256_hex(format!("{}\n{}", normalized.to_string_lossy(), branch).as_bytes())
}

fn read_cache(cache_path: &Path) -> PrCache {
    util::read_string(cache_path)
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(|cache: &PrCache| cache.v == 1)
        .unwrap_or_default()
}

fn write_cache(cache_path: &Path, mut cache: PrCache) {
    if cache.entries.len() > PR_CACHE_MAX_ENTRIES {
        let mut keys: Vec<String> = cache.entries.keys().cloned().collect();
        keys.sort_by_key(|k| cache.entries[k].fetched_at);
        let drop = cache.entries.len() - PR_CACHE_MAX_ENTRIES;
        for key in keys.into_iter().take(drop) {
            cache.entries.remove(&key);
        }
    }
    if let Ok(text) = serde_json::to_string(&cache) {
        let _ = util::atomic_write(cache_path, text.as_bytes());
    }
}

/// Hot-path lookup: a fresh entry resolves (0 → None), anything else is
/// stale so the caller can arrange a background refresh.
pub fn lookup_pr(cwd: &str, branch: &str, cache_path: &Path, now: u64) -> PrLookup {
    let cache = read_cache(cache_path);
    match cache.entries.get(&cache_key(cwd, branch)) {
        Some(entry) if now.saturating_sub(entry.fetched_at) < PR_TTL_MS => PrLookup {
            info: (entry.number > 0).then(|| PrInfo {
                number: entry.number,
                url: entry.url.clone(),
                state: entry.state.clone(),
            }),
            stale: false,
        },
        _ => PrLookup { info: None, stale: true },
    }
}

/// Spawn the detached `--refresh-pr` child when stale; the lock (shared
/// machinery with the quota refresh) dedupes concurrent frames.
pub fn ensure_fresh_pr(lock_path: &Path, cwd: &str, branch: &str, stale: bool) -> bool {
    if !stale || cwd.is_empty() || branch.is_empty() {
        return false;
    }
    let Some(token) = acquire_quota_lock(lock_path, util::now_ms()) else {
        return false;
    };
    let Ok(exe) = std::env::current_exe() else {
        release_quota_lock(lock_path, Some(&token));
        return false;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("--refresh-pr")
        .env("KIMI_HUD_RS_PR_LOCK_TOKEN", &token)
        .env("KIMI_HUD_RS_PR_CWD", cwd)
        .env("KIMI_HUD_RS_PR_BRANCH", branch)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.spawn() {
        Ok(_child) => true,
        Err(_) => {
            release_quota_lock(lock_path, Some(&token));
            false
        }
    }
}

/// `--refresh-pr` body: one `gh pr view`, every outcome cached, lock
/// released. Silent by contract (detached, output discarded).
pub fn refresh_pr(cwd: &str, branch: &str, cache_path: &Path, lock_path: &Path, token: Option<&str>) {
    let fetched = fetch_pr(cwd);
    let mut cache = read_cache(cache_path);
    cache.v = 1;
    cache.entries.insert(
        cache_key(cwd, branch),
        PrEntry {
            number: fetched.as_ref().map_or(0, |pr| pr.number),
            url: fetched.as_ref().map(|pr| pr.url.clone()).unwrap_or_default(),
            state: fetched.as_ref().map(|pr| pr.state.clone()).unwrap_or_default(),
            fetched_at: util::now_ms(),
        },
    );
    write_cache(cache_path, cache);
    release_quota_lock(lock_path, token);
}

fn fetch_pr(cwd: &str) -> Option<PrInfo> {
    let gh = resolve_command_path("gh", cwd)?;
    let mut cmd = Command::new(&gh);
    cmd.args(["pr", "view", "--json", "number,url,state"])
        .current_dir(cwd)
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_PROMPT_DISABLED", "1");
    let output = spawn_with_timeout(&mut cmd, Duration::from_millis(GH_SPAWN_TIMEOUT_MS))?;
    parse_pr(&output)
}

fn parse_pr(output: &str) -> Option<PrInfo> {
    let value: serde_json::Value = serde_json::from_str(output.trim()).ok()?;
    let number = value.get("number")?.as_u64().filter(|n| *n > 0)?;
    let url = value.get("url")?.as_str()?;
    let state = value.get("state")?.as_str()?.to_string();
    is_safe_http_url(url).then(|| PrInfo { number, url: url.to_string(), state })
}

/// Only http(s) URLs may enter an OSC 8 hyperlink — a parsed URL's
/// serialization percent-encodes control bytes, so nothing hostile can ride
/// along in the escape sequence.
pub fn is_safe_http_url(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(parsed) => {
            matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some_and(|h| !h.is_empty())
        }
        Err(_) => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_pull_request_json() {
        let pr = parse_pr(r#"{"number": 123, "url": "https://github.com/o/r/pull/123", "state": "OPEN"}"#);
        assert_eq!(
            pr,
            Some(PrInfo {
                number: 123,
                url: "https://github.com/o/r/pull/123".to_string(),
                state: "OPEN".to_string()
            })
        );
        assert_eq!(
            parse_pr(r#"{"number": 9, "url": "https://x/y/pull/9", "state": "MERGED"}"#)
                .map(|p| p.state),
            Some("MERGED".to_string())
        );
    }

    #[test]
    fn rejects_bad_payloads() {
        assert_eq!(parse_pr(r#"{"number": 0, "url": "https://x/y", "state": "OPEN"}"#), None);
        assert_eq!(parse_pr(r#"{"url": "https://x/y", "state": "OPEN"}"#), None);
        assert_eq!(
            parse_pr(r#"{"number": 5, "url": "https://x/y", "state": 1}"#),
            None,
            "state must be a string"
        );
        // Non-http(s) schemes never enter a hyperlink.
        assert_eq!(parse_pr(r#"{"number": 5, "url": "file:///etc/passwd"}"#), None);
        assert_eq!(parse_pr(r#"{"number": 5, "url": "javascript:alert(1)"}"#), None);
        assert_eq!(parse_pr("not json"), None);
        assert_eq!(parse_pr(r#"[{"number": 5}]"#), None);
    }

    #[test]
    fn url_whitelist() {
        assert!(is_safe_http_url("http://example.com/x"));
        assert!(is_safe_http_url("https://github.com/o/r/pull/1"));
        assert!(!is_safe_http_url("ftp://example.com"));
        assert!(!is_safe_http_url("file:///etc/passwd"));
        assert!(!is_safe_http_url("not a url"));
    }

    #[test]
    fn lookup_fresh_absence_and_staleness() {
        let dir = std::env::temp_dir().join(format!("kimi-hud-rs-pr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache_path = dir.join("pr.json");
        let cwd = dir.to_string_lossy().to_string();

        // Missing → stale, no info.
        let look = lookup_pr(&cwd, "main", &cache_path, 1_000);
        assert!(look.stale);
        assert!(look.info.is_none());

        // A fresh cached PR resolves; other branches stay stale.
        let mut cache = PrCache {
            v: 1,
            entries: HashMap::new(),
        };
        cache.entries.insert(
            cache_key(&cwd, "main"),
            PrEntry {
                number: 7,
                url: "https://github.com/o/r/pull/7".to_string(),
                state: "OPEN".to_string(),
                fetched_at: 1_000,
            },
        );
        // A cached absence for dev.
        cache.entries.insert(
            cache_key(&cwd, "dev"),
            PrEntry { number: 0, url: String::new(), state: String::new(), fetched_at: 1_000 },
        );
        let text = serde_json::to_string(&cache).unwrap();
        std::fs::write(&cache_path, &text).unwrap();

        let look = lookup_pr(&cwd, "main", &cache_path, 1_000 + PR_TTL_MS - 1);
        assert!(!look.stale);
        assert_eq!(look.info.map(|p| p.number), Some(7));
        let look = lookup_pr(&cwd, "dev", &cache_path, 1_000 + PR_TTL_MS - 1);
        assert!(!look.stale);
        assert!(look.info.is_none(), "cached absence hides the badge");

        // Past the TTL everything is stale again.
        assert!(lookup_pr(&cwd, "main", &cache_path, 1_000 + PR_TTL_MS).stale);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
