// Data-plane entry point: owns exactly one render frame and returns one
// line. The internal budget is 220ms, leaving headroom inside the host's
// 300ms status-line deadline. Fail-open everywhere: an error degrades the
// frame, never the TUI.

use std::time::{Duration, Instant};

use crate::git_status::{self, GitSummary};
use crate::management::HudConfig;
use crate::metrics;
use crate::model_config::{resolve_model_provider, MANAGED_KIMI_PROVIDER};
use crate::paths::RuntimePaths;
use crate::payload::read_stdin_payload;
use crate::quota;
use crate::render::{render_hud, resolve_slots, CwdStyle, RenderContext, DEFAULT_ITEMS};
use crate::theme::resolve_theme;
use crate::thinking::resolve_thinking_level;
use crate::util;

pub const RUNTIME_BUDGET_MS: u64 = 220;
const GIT_MIN_REMAINING_MS: u64 = 12;
const REFRESH_MIN_REMAINING_MS: u64 = 8;

fn remaining_ms(deadline: Instant) -> u64 {
    deadline.saturating_duration_since(Instant::now()).as_millis() as u64
}

fn layout_from(env_layout: Option<&str>, hud_config: &HudConfig) -> String {
    if let Some(layout) = env_layout.filter(|l| *l == "compact" || *l == "normal") {
        return layout.to_string();
    }
    hud_config
        .layout
        .as_deref()
        .filter(|l| *l == "compact" || *l == "normal")
        .unwrap_or("normal")
        .to_string()
}

/// Env `KIMI_HUD_RS_CWD` wins for both layouts (compact derives the
/// next-shorter form); otherwise the slots-resolved pair applies.
fn cwd_pair_from(env_cwd: Option<&str>, resolved: &crate::render::ResolvedSlots) -> (CwdStyle, CwdStyle) {
    if let Some(style) = CwdStyle::parse(env_cwd) {
        (style, style.compact_fallback())
    } else {
        (resolved.cwd_normal, resolved.cwd_compact)
    }
}

/// Env `KIMI_HUD_RS_ITEMS` (comma-separated) > config `items` > the default
/// host-footer order plus the HUD extras. Unknown names are kept — render
/// skips them — so a typo never blanks a slot.
fn items_from(env_items: Option<&str>, hud_config: &HudConfig) -> Vec<String> {
    if let Some(raw) = env_items.map(str::trim).filter(|s| !s.is_empty()) {
        let items: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !items.is_empty() {
            return items;
        }
    }
    hud_config
        .items
        .as_ref()
        .filter(|items| !items.is_empty())
        .cloned()
        .unwrap_or_else(|| DEFAULT_ITEMS.iter().map(|s| s.to_string()).collect())
}

fn color_from_env(no_color: bool, hud_no_color: bool) -> bool {
    !no_color && !hud_no_color
}

/// Returns (exit code, line). A None line prints nothing.
pub fn render_status_line(paths: &RuntimePaths) -> (i32, Option<String>) {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(RUNTIME_BUDGET_MS);

    let stdin_timeout = Duration::from_millis(150.min(remaining_ms(deadline).max(1)));
    let Some(payload) = read_stdin_payload(stdin_timeout, 1024 * 1024) else {
        return (0, Some("kimi-code-hud-rs".to_string()));
    };

    // One shared config snapshot per frame: every source read at most once.
    // config.json is parsed as JSONC — comments and trailing commas allowed.
    let hud_config: HudConfig = util::read_string(&paths.hud_config_path)
        .and_then(|text| serde_json::from_str(&util::strip_jsonc(&text)).ok())
        .unwrap_or_default();
    let config_toml = util::read_string(&paths.config_toml_path).unwrap_or_default();
    let tui_toml = util::read_string(&paths.tui_toml_path).unwrap_or_default();
    let quota_cache = quota::read_quota_cache(&paths.quota_cache_path);
    let now = util::now_ms();

    let mut summary = metrics::get_metrics(
        payload.session_id.as_deref(),
        &paths.sessions_root,
        &paths.hud_dir,
        deadline,
        now as i64,
    );

    let provider = resolve_model_provider(
        summary.model_alias.as_deref(),
        payload.model.as_deref(),
        &config_toml,
    );
    let mut quota_view: Option<quota::QuotaCache> = None;
    if provider.as_deref() == Some(MANAGED_KIMI_PROVIDER) {
        if let Some(cache) = &quota_cache {
            quota_view = Some(cache.clone());
        }
        if remaining_ms(deadline) >= REFRESH_MIN_REMAINING_MS {
            quota::ensure_fresh_quota(
                &paths.quota_lock_path,
                quota_cache.as_ref(),
                now,
            );
        }
    }

    let thinking = resolve_thinking_level(
        summary.thinking_level.as_deref(),
        payload.model.as_deref().unwrap_or(""),
        payload.session_id.as_deref(),
        &paths.hud_dir,
        &config_toml,
    );
    summary.thinking_level = Some(thinking.level);

    let mut git = GitSummary::default();
    if payload.git_branch.is_some() && remaining_ms(deadline) >= GIT_MIN_REMAINING_MS {
        git = git_status::git_status(
            payload.cwd.as_deref().unwrap_or(""),
            &paths.git_cache_path,
        );
    }

    let layout = layout_from(std::env::var("KIMI_HUD_RS_LAYOUT").ok().as_deref(), &hud_config);
    let resolved = resolve_slots(hud_config.slots.as_ref(), layout == "compact");
    let (cwd_style, cwd_compact) = cwd_pair_from(std::env::var("KIMI_HUD_RS_CWD").ok().as_deref(), &resolved);
    let styles = resolved.styles;
    let formats = resolved.formats;
    let items = items_from(std::env::var("KIMI_HUD_RS_ITEMS").ok().as_deref(), &hud_config);
    let color = color_from_env(
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var_os("KIMI_HUD_RS_NO_COLOR").is_some(),
    );
    let theme = resolve_theme(
        std::env::var("KIMI_HUD_RS_THEME").ok().as_deref(),
        &tui_toml,
        std::env::var("COLORFGBG").ok().as_deref(),
    );
    let ctx = RenderContext {
        payload: &payload,
        quota: quota_view.as_ref(),
        metrics: &summary,
        git,
        items: &items,
        cwd_style,
        cwd_compact,
        styles: &styles,
        formats: &formats,
        layout: &layout,
        color,
        theme,
        now,
    };
    (0, Some(render_hud(&ctx)))
}
