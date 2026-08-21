// Composing the one HUD line: badges, model+thinking, project+git, speed
// (fleet TPS / gen clock / compaction / TTFT), cache hit rate and quota
// bars. Degrades normal -> compact when the line exceeds 200 visible chars.

use crate::metrics::MetricsSummary;
use crate::payload::Payload;
use crate::quota::QuotaCache;
use crate::theme::Theme;
use crate::util::{sanitize_terminal_text, strip_ansi_sgr};

const ESC: &str = "\x1b[";
const RESET: &str = "\x1b[0m";
const BAR_WIDTH: usize = 10;
const MAX_WIDTH: usize = 200;

fn rgb(r: u8, g: u8, b: u8) -> String {
    format!("{}38;2;{};{};{}m", ESC, r, g, b)
}

/// The theme-dependent slots. ANSI slots are theme-independent (the terminal
/// remaps them per its own theme); badges and bar levels follow the resolved
/// theme. Base hex values mirror the host's dark/light palettes.
pub struct Palette {
    pub bright_red: String,
    pub muted: String,
    pub warning: String,
    pub primary: String,
    pub accent: String,
    pub bar_red: String,
    pub bar_yellow: String,
    pub bar_green: String,
}

fn dark_palette() -> Palette {
    Palette {
        bright_red: "\x1b[91m".to_string(),
        muted: "\x1b[90m".to_string(),
        warning: rgb(232, 168, 56),   // #E8A838 — auto/yolo badges
        primary: rgb(79, 168, 255),   // #4FA8FF — model/plan badge
        accent: rgb(91, 192, 190),    // #5BC0BE — swarm badge
        bar_red: "\x1b[31m".to_string(),
        bar_yellow: "\x1b[33m".to_string(),
        bar_green: "\x1b[32m".to_string(),
    }
}

/// Light theme: badges go bold — short labels need the extra weight on a
/// white background; the bar takes the host's calmer light error/success
/// hues instead of the terminal's glaring ANSI red.
fn light_palette() -> Palette {
    Palette {
        bright_red: "\x1b[1;91m".to_string(),
        muted: "\x1b[90m".to_string(),
        warning: format!("\x1b[1m{}", rgb(217, 119, 6)),   // bold #D97706
        primary: format!("\x1b[1m{}", rgb(21, 101, 192)),   // bold #1565C0
        accent: format!("\x1b[1m{}", rgb(20, 184, 166)),    // bold #14B8A6
        bar_red: rgb(185, 28, 28),   // #B91C1C — host light error
        bar_yellow: rgb(217, 119, 6), // #D97706
        bar_green: rgb(14, 122, 56),  // #0E7A38 — host light success
    }
}

fn palette_for(theme: Theme) -> Palette {
    match theme {
        Theme::Dark => dark_palette(),
        Theme::Light => light_palette(),
    }
}

fn colorize(enabled: bool, color: &str, text: &str) -> String {
    if enabled {
        format!("{}{}{}", color, text, RESET)
    } else {
        text.to_string()
    }
}

/// Usage-graded bar color: <60% green, <85% yellow, >=85% red.
fn level_color(fraction: f64, palette: &Palette) -> String {
    if fraction >= 0.85 {
        palette.bar_red.to_string()
    } else if fraction >= 0.6 {
        palette.bar_yellow.clone()
    } else {
        palette.bar_green.clone()
    }
}

/// Compact-mode percentage color: same thresholds, but the comfortable green
/// level stays default-colored.
fn number_level_color(fraction: f64, palette: &Palette) -> Option<String> {
    if fraction >= 0.85 {
        Some(palette.bar_red.to_string())
    } else if fraction >= 0.6 {
        Some(palette.bar_yellow.clone())
    } else {
        None
    }
}

fn bar(fraction: f64, color: bool, palette: &Palette) -> String {
    let clamped = fraction.clamp(0.0, 1.0);
    if !clamped.is_finite() {
        return String::new();
    }
    let filled = (clamped * BAR_WIDTH as f64).floor() as usize;
    let cells: String = "█".repeat(filled.min(BAR_WIDTH))
        + &"░".repeat(BAR_WIDTH - filled.min(BAR_WIDTH));
    colorize(color, &level_color(clamped, palette), &cells)
}

/// Reset countdown from an ISO timestamp: "~2h18m" / "~3d2h" / "~reset".
fn format_countdown(reset_at: Option<&str>, now: u64) -> Option<String> {
    let reset_at = reset_at?;
    let millis = parse_iso_millis(reset_at)?;
    let diff = millis as i64 - now as i64;
    if diff <= 0 {
        return Some("~reset".to_string());
    }
    let mins = diff / 60_000;
    let d = mins / 1440;
    let h = (mins % 1440) / 60;
    let m = mins % 60;
    Some(if d > 0 {
        format!("~{}d{}h", d, h)
    } else if h > 0 {
        format!("~{}h{}m", h, m)
    } else {
        format!("~{}m", m)
    })
}

/// Minimal ISO-8601 parser for the reset timestamps the quota API returns
/// (e.g. "2026-08-21T09:00:00Z", optional fractional seconds and offset).
fn parse_iso_millis(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: i64 = value.get(5..7)?.parse().ok()?;
    let day: i64 = value.get(8..10)?.parse().ok()?;
    let hour: i64 = value.get(11..13)?.parse().ok()?;
    let minute: i64 = value.get(14..16)?.parse().ok()?;
    let second: i64 = value.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut rest = &value[19..];
    let mut millis = 0i64;
    if rest.starts_with('.') {
        let end = rest[1..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|p| 1 + p + 1)
            .unwrap_or(rest.len());
        let fraction = &rest[1..end];
        millis = match fraction.len() {
            0 => 0,
            1 => fraction.parse::<i64>().ok()? * 100,
            2 => fraction.parse::<i64>().ok()? * 10,
            _ => fraction.get(0..3)?.parse::<i64>().ok()?,
        };
        rest = &rest[end..];
    }
    let mut offset_secs: i64 = 0;
    if let Some(offset) = rest.strip_prefix(['+', '-']) {
        let sign: i64 = if rest.starts_with('-') { -1 } else { 1 };
        let oh: i64 = offset.get(0..2)?.parse().ok()?;
        let om: i64 = if offset.len() >= 5 { offset.get(3..5)?.parse().ok()? } else { 0 };
        offset_secs = sign * (oh * 3600 + om * 60);
    }
    Some((civil_to_unix_millis(year, month, day, hour, minute, second, millis)
        - offset_secs * 1000) as u64)
}

/// Days-from-civil algorithm (Howard Hinnant), no external time crate.
fn civil_to_unix_millis(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    millis: i64,
) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days * 86_400 + hour * 3_600 + minute * 60 + second) * 1000 + millis
}

fn format_ttft(ms: Option<f64>) -> Option<String> {
    let ms = ms?;
    if ms < 0.0 {
        return None;
    }
    Some(if ms >= 1000.0 {
        format!("{:.1}s", ms / 1000.0)
    } else {
        format!("{}ms", ms.round())
    })
}

/// Elapsed wall-clock for the live generation ticker.
fn format_elapsed(ms: i64) -> String {
    if ms < 0 {
        return "0s".to_string();
    }
    let s = ms / 1000;
    if s < 60 {
        return format!("{}s", s);
    }
    let mins = s / 60;
    if mins < 60 {
        return format!("{}m{}s", mins, s % 60);
    }
    let h = mins / 60;
    if h < 24 {
        return format!("{}h{}m", h, mins % 60);
    }
    format!("{}d{}h", h / 24, h % 24)
}

fn pct_of(used: f64, limit: f64) -> i64 {
    ((used / limit) * 100.0).round() as i64
}

fn fleet_label(count: u32, includes_main: bool) -> String {
    if includes_main {
        format!("main+{} agents", count - 1)
    } else {
        format!("{} {}", count, if count == 1 { "agent" } else { "agents" })
    }
}

pub struct RenderContext<'a> {
    pub payload: &'a Payload,
    pub quota: Option<&'a QuotaCache>,
    pub metrics: &'a MetricsSummary,
    pub git_dirty: bool,
    pub layout: &'a str,
    pub color: bool,
    pub theme: Theme,
    pub now: u64,
}

fn badges(ctx: &RenderContext, palette: &Palette) -> Vec<String> {
    let mut out = Vec::new();
    let mode = ctx.payload.permission_mode.as_deref();
    if mode == Some("yolo") {
        out.push(colorize(ctx.color, &palette.warning, "[yolo]"));
    } else if mode == Some("auto") {
        out.push(colorize(ctx.color, &palette.bright_red, "[auto]"));
    } else {
        out.push(colorize(ctx.color, &palette.muted, "[manual]"));
    }
    if ctx.payload.plan_mode.unwrap_or(false) {
        out.push(colorize(ctx.color, &palette.primary, "[plan]"));
    }
    if ctx.metrics.swarm_mode || ctx.payload.swarm_mode.unwrap_or(false) {
        out.push(colorize(ctx.color, &palette.accent, "[swarm]"));
    }
    out
}

fn model_segment(ctx: &RenderContext, palette: &Palette, compact: bool) -> String {
    let level = ctx
        .metrics
        .thinking_level
        .as_deref()
        .filter(|l| !l.is_empty())
        .map(sanitize_terminal_text);
    let mut segment = colorize(
        ctx.color,
        &palette.primary,
        &sanitize_terminal_text(ctx.payload.model.as_deref().unwrap_or("")),
    );
    if let Some(level) = level.filter(|l| l != "off") {
        // Effort-capable models show the bare level; boolean thinking keeps
        // the " thinking" label (compact: " on"). An unconfirmed inference
        // renders muted, like the provisional TPS reading.
        let suffix = if level == "on" && !compact {
            " thinking".to_string()
        } else {
            format!(" {}", level)
        };
        segment.push_str(&if ctx.metrics.thinking_provisional {
            colorize(ctx.color, &palette.muted, &suffix)
        } else {
            suffix
        });
    }
    segment
}

fn project_segment(ctx: &RenderContext, compact: bool) -> Option<String> {
    let cwd = ctx.payload.cwd.as_deref().map(sanitize_terminal_text).unwrap_or_default();
    let project = if !compact && !cwd.is_empty() {
        Some(
            cwd.rsplit(['/', '\\'])
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(&cwd)
                .to_string(),
        )
    } else {
        None
    };
    let branch = ctx.payload.git_branch.as_deref().map(sanitize_terminal_text);
    match branch.filter(|b| !b.is_empty()) {
        Some(branch) => {
            let git = format!("git:({}{})", branch, if ctx.git_dirty { "*" } else { "" });
            Some(match project {
                Some(project) => format!("{} {}", project, git),
                None => git,
            })
        }
        None => project,
    }
}

fn speed_segment(ctx: &RenderContext, palette: &Palette, compact: bool) -> Option<String> {
    let metrics = ctx.metrics;
    let now = ctx.now as i64;
    let live_subagents = metrics
        .active_agents
        .saturating_sub(u32::from(metrics.main_active));
    let multi = metrics.tps_agents > 1 || (metrics.swarm_mode && live_subagents >= 1 && metrics.tps_agents >= 1);
    let generated_for = metrics
        .turn_started_at
        .filter(|t| now >= *t)
        .map(|t| format_elapsed(now - t));
    let compacting = if generated_for.is_none() {
        metrics.compacting_since.map(|t| format_elapsed(now.max(t) - t))
    } else {
        None
    };
    let compacted = if generated_for.is_none() && compacting.is_none() {
        metrics.compaction_ms.map(|ms| format_elapsed(ms))
    } else {
        None
    };

    if let Some(tps) = metrics.tps {
        let average = tps.round();
        let paint = |text: &str| -> String {
            if metrics.tps_stale {
                colorize(ctx.color, &palette.muted, text)
            } else {
                text.to_string()
            }
        };
        if compact {
            let count = if multi && metrics.main_speed {
                format!("main+{}", metrics.tps_agents - 1)
            } else {
                metrics.tps_agents.to_string()
            };
            let head = if multi {
                format!("⚡ {} ({}@{})", metrics.tps_total.map(|t| t.round()).unwrap_or(average), count, average)
            } else {
                format!("⚡ {}", average)
            };
            let live = if let Some(gen_text) = &generated_for {
                format!("gen {}", gen_text)
            } else if let Some(c) = &compacting {
                format!("compacting {}", c)
            } else {
                return Some(paint(&head));
            };
            return Some(format!("{} {}", paint(&head), live));
        }
        let base = if multi {
            format!(
                "⚡ {} t/s ({} @{})",
                metrics.tps_total.map(|t| t.round()).unwrap_or(average),
                fleet_label(metrics.tps_agents, metrics.main_speed),
                average
            )
        } else {
            format!("⚡ {} t/s", average)
        };
        if let Some(gen_text) = &generated_for {
            return Some(format!("{} · gen {}", paint(&base), gen_text));
        }
        if let Some(c) = &compacting {
            return Some(format!("{} · compacting {}", paint(&base), c));
        }
        if let Some(c) = &compacted {
            return Some(format!("{}{}", paint(&base), colorize(ctx.color, &palette.muted, &format!(" · compacted {}", c))));
        }
        return match format_ttft(metrics.ttft_ms) {
            Some(ttft) => Some(paint(&format!("{} · TTFT {}", base, ttft))),
            None => Some(paint(&base)),
        };
    }
    if let (Some(_), Some(gen_text)) = (metrics.turn_started_at, &generated_for) {
        let agents = if metrics.active_agents > 1 || (metrics.swarm_mode && live_subagents >= 1) {
            format!(" ({})", fleet_label(metrics.active_agents, metrics.main_active))
        } else {
            String::new()
        };
        return Some(format!("⚡ gen {}{}", gen_text, agents));
    }
    if let Some(c) = &compacting {
        return Some(format!("compacting {}", c));
    }
    if let Some(c) = &compacted {
        if !compact {
            return Some(colorize(ctx.color, &palette.muted, &format!("compacted {}", c)));
        }
    }
    format_ttft(metrics.ttft_ms).map(|ttft| format!("TTFT {}", ttft))
}

fn cache_segment(ctx: &RenderContext) -> Option<String> {
    let hit_rate = ctx.metrics.cache.as_ref()?.hit_rate;
    if !hit_rate.is_finite() || !(0.0..=1.0).contains(&hit_rate) {
        return None;
    }
    Some(format!("Cache {}%", (hit_rate * 100.0).round() as i64))
}

fn quota_segment(ctx: &RenderContext, palette: &Palette, compact: bool) -> Option<String> {
    let quota = ctx.quota?;
    let mut parts: Vec<String> = Vec::new();
    for window in &quota.windows {
        let fraction = window.used / window.limit;
        let pct = format!("{}%", pct_of(window.used, window.limit));
        let label = sanitize_terminal_text(&window.label);
        let mut text = if compact {
            match number_level_color(fraction, palette) {
                Some(level) => format!("{} {}", label, colorize(ctx.color, &level, &pct)),
                None => format!("{} {}", label, pct),
            }
        } else {
            format!("{} {} {}", label, bar(fraction, ctx.color, palette), pct)
        };
        if let Some(countdown) = format_countdown(window.reset_at.as_deref(), ctx.now) {
            text.push_str(&format!(" {}", countdown));
        }
        parts.push(text);
    }
    if !compact {
        if let Some(weekly) = &quota.weekly {
            let fraction = weekly.used / weekly.limit;
            let mut text = format!(
                "7d {} {}%",
                bar(fraction, ctx.color, palette),
                pct_of(weekly.used, weekly.limit)
            );
            if let Some(countdown) = format_countdown(weekly.reset_at.as_deref(), ctx.now) {
                text.push_str(&format!(" {}", countdown));
            }
            parts.push(text);
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Render the HUD line. Downgrades normal -> compact above 200 visible
/// characters; the final fallback is the sanitized model name.
pub fn render_hud(ctx: &RenderContext) -> String {
    let palette = palette_for(ctx.theme);
    let layouts: [&str; 2] = ["normal", "compact"];
    let start = layouts
        .iter()
        .position(|l| *l == ctx.layout)
        .unwrap_or(0);
    for layout in &layouts[start..] {
        let compact = *layout == "compact";
        let prefix = badges(ctx, &palette);
        let mut segments: Vec<String> = Vec::new();
        let model = model_segment(ctx, &palette, compact);
        if !model.is_empty() {
            segments.push(model);
        }
        if let Some(seg) = project_segment(ctx, compact) {
            segments.push(seg);
        }
        if let Some(seg) = speed_segment(ctx, &palette, compact) {
            segments.push(seg);
        }
        if let Some(seg) = cache_segment(ctx) {
            segments.push(seg);
        }
        if let Some(seg) = quota_segment(ctx, &palette, compact) {
            segments.push(seg);
        }
        let mut line_parts = prefix;
        if !segments.is_empty() {
            line_parts.push(segments.join(" │ "));
        }
        let line = line_parts.join(" ");
        if strip_ansi_sgr(&line).chars().count() <= MAX_WIDTH || compact {
            return line;
        }
    }
    sanitize_terminal_text(ctx.payload.model.as_deref().unwrap_or("kimi"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::QuotaEntry;

    fn metrics() -> MetricsSummary {
        MetricsSummary::default()
    }

    fn base_payload() -> Payload {
        Payload {
            model: Some("K3".to_string()),
            cwd: Some("/work/kimi-code-hud".to_string()),
            git_branch: Some("main".to_string()),
            permission_mode: Some("manual".to_string()),
            ..Payload::default()
        }
    }

    fn ctx<'a>(
        payload: &'a Payload,
        metrics_value: &'a MetricsSummary,
        quota: Option<&'a QuotaCache>,
    ) -> RenderContext<'a> {
        RenderContext {
            payload,
            quota,
            metrics: metrics_value,
            git_dirty: false,
            layout: "normal",
            color: false,
            theme: Theme::Dark,
            now: 1_800_000_000_000,
        }
    }

    #[test]
    fn plain_line_without_metrics() {
        let payload = base_payload();
        let m = metrics();
        let line = render_hud(&ctx(&payload, &m, None));
        assert!(line.contains("[manual]"));
        assert!(line.contains("K3"));
        assert!(line.contains("kimi-code-hud"));
        assert!(line.contains("git:(main)"));
        assert!(!line.contains("Cache"));
        assert!(!line.contains("5h"));
    }

    #[test]
    fn speed_and_cache_render() {
        let payload = base_payload();
        let mut m = metrics();
        m.tps = Some(46.7);
        m.ttft_ms = Some(1_300.0);
        m.cache = Some(crate::metrics::CacheMetric { hit_rate: 0.92 });
        let line = render_hud(&ctx(&payload, &m, None));
        assert!(line.contains("⚡ 47 t/s"));
        assert!(line.contains("TTFT 1.3s"));
        assert!(line.contains("Cache 92%"));
    }

    #[test]
    fn fleet_style_when_multiple_agents() {
        let payload = base_payload();
        let mut m = metrics();
        m.tps = Some(52.0);
        m.tps_total = Some(156.0);
        m.tps_agents = 3;
        m.active_agents = 3;
        let line = render_hud(&ctx(&payload, &m, None));
        assert!(line.contains("⚡ 156 t/s (3 agents @52)"));
    }

    #[test]
    fn gen_ticker_while_turn_in_flight() {
        let payload = base_payload();
        let mut m = metrics();
        m.turn_started_at = Some(1_799_999_000_000);
        let line = render_hud(&ctx(&payload, &m, None));
        assert!(line.contains("gen 16m40s"), "line was: {}", line);
    }

    #[test]
    fn quota_windows_and_weekly() {
        let payload = base_payload();
        let m = metrics();
        let quota = QuotaCache {
            fetched_at: 1,
            weekly: Some(QuotaEntry {
                label: String::new(),
                used: 25.0,
                limit: 100.0,
                reset_at: None,
            }),
            windows: vec![QuotaEntry {
                label: "5h".to_string(),
                used: 31.0,
                limit: 100.0,
                reset_at: Some("2100-01-01T00:00:00Z".to_string()),
            }],
        };
        let line = render_hud(&ctx(&payload, &m, Some(&quota)));
        assert!(line.contains("5h"));
        assert!(line.contains("31%"));
        assert!(line.contains("7d"));
        assert!(line.contains("25%"));
        assert!(line.contains("░"));
    }

    #[test]
    fn countdown_formats() {
        let now = 1_800_000_000_000u64;
        let later = now + 8_280_000; // 2h18m
        assert_eq!(
            format_countdown(Some(&millis_to_iso(later)), now).as_deref(),
            Some("~2h18m")
        );
        assert_eq!(format_countdown(Some("2000-01-01T00:00:00Z"), now).as_deref(), Some("~reset"));
        assert_eq!(format_countdown(None, now), None);
    }

    fn millis_to_iso(millis: u64) -> String {
        // Inverse of civil_to_unix_millis for round-tripping in tests.
        let secs = millis / 1000;
        let mut days = secs / 86_400;
        let rem = secs % 86_400;
        let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        let mut year = 1970i64;
        loop {
            let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
            let len = if leap { 366 } else { 365 };
            if days >= len {
                days -= len;
                year += 1;
            } else {
                break;
            }
        }
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let month_lengths = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut month = 0usize;
        while days >= month_lengths[month] {
            days -= month_lengths[month];
            month += 1;
        }
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            year,
            month + 1,
            days + 1,
            h,
            mi,
            s
        )
    }

    #[test]
    fn iso_parser_roundtrip() {
        assert_eq!(parse_iso_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_millis("1970-01-02T00:00:00Z"), Some(86_400_000));
        assert_eq!(parse_iso_millis("bogus"), None);
        let iso = millis_to_iso(1_800_000_000_000);
        assert_eq!(parse_iso_millis(&iso), Some(1_800_000_000_000));
    }
}

