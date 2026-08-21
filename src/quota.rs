// Managed-subscription quota (5h/7d windows). The render hot path only reads
// the 60-second cache; when it is stale it spawns one detached refresh
// process guarded by a lock file, so the 300ms frame budget is never spent
// on the network. The token only ever leaves this process toward the two
// official /usages endpoints.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model_config::{decoded_string_value, find_provider_table, managed_oauth_table};
use crate::util;

pub const USAGES_URL: &str = "https://api.kimi.com/coding/v1/usages";
#[allow(dead_code)] // used in tests and kept beside USAGES_URL for symmetry
pub const GLOBAL_USAGES_URL: &str = "https://api.kimi.ai/coding/v1/usages";
pub const QUOTA_TTL_MS: u64 = 60_000;
pub const LOCK_STALE_MS: u64 = 30_000;
const REQUEST_TIMEOUT_MS: u64 = 8_000;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QuotaEntry {
    pub label: String,
    pub used: f64,
    pub limit: f64,
    #[serde(default)]
    pub reset_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QuotaCache {
    #[serde(default)]
    pub fetched_at: u64,
    #[serde(default)]
    pub weekly: Option<QuotaEntry>,
    #[serde(default)]
    pub windows: Vec<QuotaEntry>,
}

/// Read the quota cache file; None when missing or malformed.
pub fn read_quota_cache(cache_path: &Path) -> Option<QuotaCache> {
    let text = util::read_string(cache_path)?;
    serde_json::from_str::<QuotaCache>(&text).ok()
}

pub fn is_stale(cache: Option<&QuotaCache>, now: u64) -> bool {
    match cache {
        Some(cache) => now.saturating_sub(cache.fetched_at) > QUOTA_TTL_MS,
        None => true,
    }
}

fn write_quota_cache(parsed: &QuotaCache, cache_path: &Path) {
    let mut body = parsed.clone();
    body.fetched_at = util::now_ms();
    if let Ok(text) = serde_json::to_string(&body) {
        let _ = util::atomic_write(cache_path, text.as_bytes());
    }
}

/// Lenient numeric read like the Node port's Number(): JSON numbers, or
/// numeric strings ("31"), both accepted.
fn to_num(value: Option<&Value>) -> Option<f64> {
    let v = value?;
    let n = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    n.is_finite().then_some(n)
}

fn quota_values(detail: &Value) -> (Option<f64>, Option<f64>) {
    let limit = to_num(detail.get("limit"));
    let mut used = to_num(detail.get("used"));
    if used.is_none() {
        // Bonus/overflow quota can report remaining > limit; clamp to zero
        // usage instead of rejecting, but fail closed on negative data.
        if let (Some(limit), Some(remaining)) = (limit, to_num(detail.get("remaining"))) {
            if remaining >= 0.0 {
                used = Some((limit - remaining).clamp(0.0, limit));
            }
        }
    }
    (used, limit)
}

/// "300 minutes" -> "5h"; hours/days stay compact.
fn derive_window_label(window: &Value) -> Option<String> {
    let duration = window.get("duration")?.as_f64()?;
    if !duration.is_finite() || duration <= 0.0 {
        return None;
    }
    let d = duration as i64;
    match window.get("timeUnit").and_then(|v| v.as_str())? {
        "TIME_UNIT_MINUTE" => {
            if d % 1440 == 0 {
                Some(format!("{}d", d / 1440))
            } else if d % 60 == 0 {
                Some(format!("{}h", d / 60))
            } else {
                Some(format!("{}m", d))
            }
        }
        "TIME_UNIT_HOUR" => {
            if d % 24 == 0 {
                Some(format!("{}d", d / 24))
            } else {
                Some(format!("{}h", d))
            }
        }
        "TIME_UNIT_DAY" => Some(format!("{}d", d)),
        _ => None,
    }
}

/// Parse the /usages API response. Lenient like the Node port: numeric fields
/// may be strings, missing zero usage is derived from limit - remaining, and
/// the detail may live on the item top level. None when nothing usable.
pub fn parse_quota_payload(json: &Value) -> Option<QuotaCache> {
    if !json.is_object() {
        return None;
    }
    let mut weekly = None;
    if let Some(usage) = json.get("usage").filter(|u| u.is_object()) {
        let (used, limit) = quota_values(usage);
        if let (Some(used), Some(limit)) = (used, limit) {
            if limit > 0.0 {
                weekly = Some(QuotaEntry {
                    label: String::new(),
                    used,
                    limit,
                    reset_at: usage.get("resetTime").and_then(|v| v.as_str()).map(str::to_string),
                });
            }
        }
    }
    let mut windows = Vec::new();
    if let Some(limits) = json.get("limits").and_then(|v| v.as_array()) {
        for item in limits {
            let Some(item) = item.as_object() else { continue };
            let detail_owned;
            let detail = match item.get("detail").filter(|d| d.is_object()) {
                Some(d) => d,
                None => {
                    detail_owned = Value::Object(item.clone());
                    &detail_owned
                }
            };
            let (used, limit) = quota_values(detail);
            let label = item.get("window").and_then(derive_window_label);
            let (Some(used), Some(limit), Some(label)) = (used, limit, label) else {
                continue;
            };
            if limit <= 0.0 {
                continue;
            }
            windows.push(QuotaEntry {
                label,
                used,
                limit,
                reset_at: detail.get("resetTime").and_then(|v| v.as_str()).map(str::to_string),
            });
        }
    }
    if weekly.is_none() && windows.is_empty() {
        return None;
    }
    Some(QuotaCache {
        fetched_at: 0,
        weekly,
        windows,
    })
}

// --- Detached-refresh lock -------------------------------------------------

fn lock_token(now: u64) -> String {
    // Random-ish without a dependency: the hasher seeds itself randomly.
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(now);
    hasher.write_u64(u64::from(std::process::id() as u32));
    format!("{:016x}", hasher.finish())
}

fn read_lock(lock_path: &Path) -> Option<Value> {
    serde_json::from_str::<Value>(&util::read_string(lock_path)?)
        .ok()
        .filter(|v| v.is_object())
}

/// Atomically acquire the detached-refresh lock. A stale lock is renamed out
/// of the way first, then contenders race on an atomic hard link, so the lock
/// never appears with partial content.
pub fn acquire_quota_lock(lock_path: &Path, now: u64) -> Option<String> {
    let dir = lock_path.parent()?;
    fs::create_dir_all(dir).ok()?;
    if let Some(current) = read_lock(lock_path) {
        let at = current.get("at").and_then(|v| v.as_u64()).unwrap_or(0);
        if now.saturating_sub(at) < LOCK_STALE_MS {
            return None;
        }
        // Stale: move it aside (best effort) and race below.
        let stale = dir.join(format!("{}.stale", lock_path.file_name()?.to_string_lossy()));
        let _ = fs::rename(lock_path, &stale);
        let _ = fs::remove_file(&stale);
    }
    let token = lock_token(now);
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        lock_path.file_name()?.to_string_lossy(),
        token
    ));
    let body = serde_json::json!({ "pid": std::process::id(), "at": now, "token": token });
    let result = (|| -> Option<()> {
        fs::write(&tmp, body.to_string()).ok()?;
        match fs::hard_link(&tmp, lock_path) {
            Ok(()) => Some(()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => None,
            Err(_) => None,
        }
    })();
    let _ = fs::remove_file(&tmp);
    result?;
    Some(token)
}

/// Remove the lock only when it is still owned by the supplied refresh.
pub fn release_quota_lock(lock_path: &Path, token: Option<&str>) -> bool {
    let Some(current) = read_lock(lock_path) else {
        return false;
    };
    match token {
        Some(token) => {
            if current.get("token").and_then(|v| v.as_str()) != Some(token) {
                return false;
            }
        }
        None => {
            if current.get("token").and_then(|v| v.as_str()).is_some() {
                return false;
            }
        }
    }
    fs::remove_file(lock_path).is_ok()
}

/// If the cache is stale, spawn one detached refresh and return immediately.
/// Never blocks on the network; never prints.
pub fn ensure_fresh_quota(
    lock_path: &Path,
    cached: Option<&QuotaCache>,
    now: u64,
) -> bool {
    if !is_stale(cached, now) {
        return false;
    }
    let Some(token) = acquire_quota_lock(lock_path, now) else {
        return false;
    };
    let Ok(exe) = std::env::current_exe() else {
        release_quota_lock(lock_path, Some(&token));
        return false;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("--refresh-quota")
        .env("KIMI_HUD_RS_QUOTA_LOCK_TOKEN", &token)
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

// --- Region resolution -----------------------------------------------------

const OFFICIAL_USAGES_HOSTS: [&str; 2] = ["api.kimi.com", "api.kimi.ai"];

// Dual-region model (Kimi Code 0.38.0): mainland-cn (default) and global.
struct RegionProfile {
    oauth_host: &'static str,
    base_url: &'static str,
}

const REGION_PROFILES: [RegionProfile; 2] = [
    RegionProfile { oauth_host: "https://auth.kimi.com", base_url: "https://api.kimi.com/coding/v1" },
    RegionProfile { oauth_host: "https://auth.kimi.ai", base_url: "https://api.kimi.ai/coding/v1" },
];

fn normalize_endpoint(value: Option<&str>) -> Option<String> {
    let normalized = value?.trim().trim_end_matches('/').to_string();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// Map an oauth ref key to its credential file. Only the two upstream key
/// shapes are honored; the whitelist doubles as path sanitization.
fn credentials_path_for_key(key: Option<&str>, credentials_dir: &Path) -> PathBuf {
    let valid = key.and_then(|key| {
        let rest = key.strip_prefix("oauth/")?;
        let name = rest
            .strip_prefix("kimi-code")
            .map(|suffix| format!("kimi-code{}", suffix))?;
        let ok = name == "kimi-code"
            || (name.starts_with("kimi-code-env-")
                && name["kimi-code-env-".len()..].len() == 16
                && name["kimi-code-env-".len()..]
                    .chars()
                    .all(|c| c.is_ascii_hexdigit()));
        ok.then_some(name)
    });
    credentials_dir.join(format!(
        "{}.json",
        valid.as_deref().unwrap_or("kimi-code")
    ))
}

#[derive(Debug, Clone)]
pub struct QuotaEndpoints {
    pub credentials_path: PathBuf,
    pub url: String,
}

/// Resolve which credential file to read and which official /usages URL to
/// call. Fail closed: the URL only ever leaves here as one of the two
/// official region endpoints — any custom oauth host or base_url (internal
/// proxies, mirrors, typos) and any host/base pair pinned to different
/// regions falls back to the mainland default.
pub fn resolve_quota_endpoints(
    env_oauth_host: Option<&str>,
    env_base_url: Option<&str>,
    config_text: &str,
    kimi_home: &Path,
) -> QuotaEndpoints {
    let credentials_dir = kimi_home.join("credentials");
    let fallback = QuotaEndpoints {
        credentials_path: credentials_dir.join("kimi-code.json"),
        url: USAGES_URL.to_string(),
    };
    let provider_table = find_provider_table(config_text, crate::model_config::MANAGED_KIMI_PROVIDER);
    let oauth_table = managed_oauth_table(config_text);
    let configured_base_url = provider_table
        .as_deref()
        .and_then(|t| decoded_string_value(t, "base_url"));
    let configured_oauth_host = oauth_table
        .as_deref()
        .and_then(|t| decoded_string_value(t, "oauth_host"));
    let configured_key = oauth_table.as_deref().and_then(|t| decoded_string_value(t, "key"));

    let host = normalize_endpoint(env_oauth_host).or_else(|| normalize_endpoint(configured_oauth_host.as_deref()));
    let profile = host.as_ref().and_then(|h| {
        REGION_PROFILES
            .iter()
            .find(|p| p.oauth_host == h)
    });
    if host.is_some() && profile.is_none() {
        return fallback;
    }
    let base_url = normalize_endpoint(env_base_url)
        .or_else(|| normalize_endpoint(configured_base_url.as_deref()))
        .or_else(|| Some(profile.map(|p| p.base_url).unwrap_or(REGION_PROFILES[0].base_url).to_string()))
        .unwrap();
    let endpoint_profile = REGION_PROFILES.iter().find(|p| p.base_url == base_url);
    let Some(endpoint_profile) = endpoint_profile else {
        return fallback;
    };
    if let Some(profile) = profile {
        if profile.oauth_host != endpoint_profile.oauth_host {
            return fallback;
        }
    }
    QuotaEndpoints {
        credentials_path: credentials_path_for_key(configured_key.as_deref(), &credentials_dir),
        url: format!("{}/usages", endpoint_profile.base_url),
    }
}

/// Second whitelist pass right before any token is sent: https, an official
/// host, default port, the exact /usages path, and no embedded credentials.
fn official_usages_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed
            .host_str()
            .is_some_and(|host| OFFICIAL_USAGES_HOSTS.contains(&host))
        && parsed.port().is_none()
        && parsed.path() == "/coding/v1/usages"
        && parsed.username().is_empty()
        && parsed.password().is_none()
}

// --- HTTP refresh (--refresh-quota child) ----------------------------------

enum QuotaOutcome {
    Success(QuotaCache),
    Unauthorized,
    Transient,
    Invalid,
}

fn request_quota(token: &str, url: &str, timeout_ms: u64) -> QuotaOutcome {
    if token.is_empty() || !official_usages_url(url) {
        return QuotaOutcome::Invalid;
    }
    let response = ureq::get(url)
        .timeout(Duration::from_millis(timeout_ms))
        .set("Authorization", &format!("Bearer {}", token))
        .set("Accept", "application/json")
        .call();
    let (status, body) = match response {
        Ok(resp) => (resp.status(), resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
        Err(_) => return QuotaOutcome::Transient,
    };
    if status == 401 || status == 403 {
        return QuotaOutcome::Unauthorized;
    }
    if status == 429 || status >= 500 {
        return QuotaOutcome::Transient;
    }
    if !(200..300).contains(&status) {
        return QuotaOutcome::Invalid;
    }
    match serde_json::from_str::<Value>(&body) {
        Ok(json) => match parse_quota_payload(&json) {
            Some(parsed) => QuotaOutcome::Success(parsed),
            None => QuotaOutcome::Invalid,
        },
        Err(_) => QuotaOutcome::Invalid,
    }
}

/// --refresh-quota entry point: read credentials, call /usages, write cache.
/// Completely silent on success and failure. When the credentials are gone or
/// carry no token (/logout), the stale cache is deleted so the HUD stops
/// rendering quota; a 401 with a refresh_token still present is only an
/// expired access_token (the CLI refreshes lazily), so the cache survives.
pub fn refresh_quota(
    endpoints: &QuotaEndpoints,
    cache_path: &Path,
    lock_path: &Path,
    lock_token: Option<&str>,
) -> bool {
    let result = (|| -> bool {
        let cred: Option<serde_json::Value> = util::read_string(&endpoints.credentials_path)
            .and_then(|text| serde_json::from_str(&text).ok());
        let token = cred
            .as_ref()
            .and_then(|c| c.get("access_token"))
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty());
        let Some(token) = token else {
            let _ = fs::remove_file(cache_path);
            return false;
        };
        match request_quota(token, &endpoints.url, REQUEST_TIMEOUT_MS) {
            QuotaOutcome::Unauthorized => {
                let can_refresh = cred
                    .as_ref()
                    .and_then(|c| c.get("refresh_token"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| !t.is_empty());
                if !can_refresh {
                    let _ = fs::remove_file(cache_path);
                }
                false
            }
            QuotaOutcome::Success(parsed) => {
                write_quota_cache(&parsed, cache_path);
                true
            }
            _ => false,
        }
    })();
    release_quota_lock(lock_path, lock_token);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_windows_and_weekly() {
        let json = serde_json::json!({
            "usage": {"used": 40, "limit": 100, "resetTime": "2026-08-21T00:00:00Z"},
            "limits": [
                {"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                 "detail": {"used": "31", "limit": "100", "resetTime": "2026-08-21T00:00:00Z"}},
                {"window": {"duration": 7, "timeUnit": "TIME_UNIT_DAY"}, "detail": {"limit": 10, "remaining": 4}}
            ]
        });
        let parsed = parse_quota_payload(&json).unwrap();
        assert_eq!(parsed.windows.len(), 2);
        assert_eq!(parsed.windows[0].label, "5h");
        assert_eq!(parsed.windows[0].used, 31.0);
        assert_eq!(parsed.windows[1].label, "7d");
        assert_eq!(parsed.windows[1].used, 6.0);
        assert!(parsed.weekly.is_some());
    }

    #[test]
    fn remaining_over_limit_clamps_to_zero() {
        let json = serde_json::json!({
            "limits": [
                {"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                 "detail": {"limit": 100, "remaining": 150}}
            ]
        });
        let parsed = parse_quota_payload(&json).unwrap();
        assert_eq!(parsed.windows[0].used, 0.0);
    }

    #[test]
    fn rejects_empty_payload() {
        assert!(parse_quota_payload(&serde_json::json!({})).is_none());
        assert!(parse_quota_payload(&serde_json::json!({"limits": []})).is_none());
    }

    #[test]
    fn endpoint_resolution_fails_closed() {
        let config = r#"
[providers."managed:kimi-code"]
base_url = "https://api.kimi.com/coding/v1"
[providers."managed:kimi-code".oauth]
oauth_host = "https://auth.kimi.com"
"#;
        let resolved = resolve_quota_endpoints(None, None, config, Path::new("/home/.kimi-code"));
        assert_eq!(resolved.url, USAGES_URL);

        // A custom oauth host falls back to the default instead of leaking.
        let custom = resolve_quota_endpoints(Some("https://evil.example"), None, config, Path::new("/h"));
        assert_eq!(custom.url, USAGES_URL);

        // Global region switches both URL and credential slot.
        let global_config = r#"
[providers."managed:kimi-code"]
base_url = "https://api.kimi.ai/coding/v1"
[providers."managed:kimi-code".oauth]
key = "oauth/kimi-code-env-0123456789abcdef"
oauth_host = "https://auth.kimi.ai"
"#;
        let global = resolve_quota_endpoints(None, None, global_config, Path::new("/h"));
        assert_eq!(global.url, GLOBAL_USAGES_URL);
        assert_eq!(
            global.credentials_path,
            Path::new("/h/credentials/kimi-code-env-0123456789abcdef.json")
        );
    }

    #[test]
    fn url_whitelist() {
        assert!(official_usages_url(USAGES_URL));
        assert!(official_usages_url(GLOBAL_USAGES_URL));
        assert!(!official_usages_url("http://api.kimi.com/coding/v1/usages"));
        assert!(!official_usages_url("https://api.kimi.com:8443/coding/v1/usages"));
        assert!(!official_usages_url("https://evil.example/coding/v1/usages"));
        assert!(!official_usages_url("https://user:pw@api.kimi.com/coding/v1/usages"));
        assert!(!official_usages_url("https://api.kimi.com/coding/v1/usages/extra"));
    }

    #[test]
    fn lock_acquire_and_release() {
        let dir = std::env::temp_dir().join(format!("kimi-hud-rs-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("refresh.lock");
        let now = util::now_ms();
        let token = acquire_quota_lock(&lock, now).unwrap();
        // A second contender within the stale window loses.
        assert!(acquire_quota_lock(&lock, now + 1000).is_none());
        assert!(release_quota_lock(&lock, Some(&token)));
        // A stale lock is replaced.
        let token2 = acquire_quota_lock(&lock, now + LOCK_STALE_MS + 1000).unwrap();
        assert!(release_quota_lock(&lock, Some(&token2)));
        // A foreign token cannot remove a token-owned lock.
        let token3 = acquire_quota_lock(&lock, now + LOCK_STALE_MS + 2000).unwrap();
        assert!(!release_quota_lock(&lock, None));
        assert!(release_quota_lock(&lock, Some(&token3)));
        let _ = fs::remove_dir_all(&dir);
    }
}
