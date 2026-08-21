// Runtime path resolution. Kimi-owned files live under the host's home
// (~/.kimi-code by default); everything this HUD writes lives under its own
// ~/.kimi-code-hud-rs directory so the Node implementation and this port can
// coexist without sharing caches or cursors.

use std::env;
use std::path::PathBuf;

fn home_dir() -> PathBuf {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub kimi_home: PathBuf,
    pub hud_dir: PathBuf,
    pub sessions_root: PathBuf,
    pub hud_config_path: PathBuf,
    pub quota_cache_path: PathBuf,
    pub quota_lock_path: PathBuf,
    pub git_cache_path: PathBuf,
    pub tui_toml_path: PathBuf,
    pub config_toml_path: PathBuf,
}

/// Resolve all paths once per process; env overrides exist for tests and
/// sandboxed setups, mirroring the Node implementation's variable names with
/// an RS suffix so both HUDs can run side by side.
impl RuntimePaths {
    pub fn resolve() -> Self {
        let home = home_dir();
        let kimi_home = env::var_os("KIMI_CODE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".kimi-code"));
        let hud_dir = env::var_os("KIMI_HUD_RS_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".kimi-code-hud-rs"));
        RuntimePaths {
            sessions_root: kimi_home.join("sessions"),
            hud_config_path: hud_dir.join("config.json"),
            quota_cache_path: hud_dir.join("quota.json"),
            quota_lock_path: hud_dir.join("quota-refresh.lock"),
            git_cache_path: hud_dir.join("git-status-cache.json"),
            tui_toml_path: env::var_os("KIMI_HUD_RS_TUI_TOML")
                .map(PathBuf::from)
                .unwrap_or_else(|| kimi_home.join("tui.toml")),
            config_toml_path: env::var_os("KIMI_HUD_RS_CONFIG_TOML")
                .map(PathBuf::from)
                .unwrap_or_else(|| kimi_home.join("config.toml")),
            kimi_home,
            hud_dir,
        }
    }
}
