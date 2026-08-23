// Composing the one HUD line in the host footer's own visual language:
// mode/model/cwd/git slots copied byte-for-byte from the built-in footer
// (two-space separated, host palette), followed by the extra HUD segments
// (fleet TPS / gen clock / compaction / TTFT, cache hit rate, quota bars).
// Slot order is configurable via config.json `items`. Degrades
// normal -> compact when the line exceeds 200 visible chars.

use serde::{Deserialize, Serialize};

use crate::git_status::GitSummary;
use crate::metrics::MetricsSummary;
use crate::payload::Payload;
use crate::quota::QuotaCache;
use crate::theme::Theme;
use crate::util::{sanitize_terminal_text, strip_ansi_sgr};

const ESC: &str = "\x1b[";
const RESET: &str = "\x1b[0m";
const BAR_WIDTH: usize = 10;
const MAX_WIDTH: usize = 200;

/// Default slot order: the host footer's four slots, then the HUD extras.
pub const DEFAULT_ITEMS: [&str; 8] = [
    "mode", "model", "tasks", "cwd", "git", "speed", "cache", "quota",
];

/// cwd slot rendering style. Short is the host footer's own abbreviation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CwdStyle {
    /// `~` + at most 3 trailing segments, `…/` prefix when longer (host default).
    #[default]
    Short,
    /// `~`-abbreviated full path.
    Full,
    /// Last path component only.
    Name,
}

impl CwdStyle {
    pub fn parse(value: Option<&str>) -> Option<CwdStyle> {
        match value {
            Some("short") => Some(CwdStyle::Short),
            // "long" aliases "full" so the shared `format` field reads uniformly.
            Some("full") | Some("long") => Some(CwdStyle::Full),
            Some("name") => Some(CwdStyle::Name),
            _ => None,
        }
    }

    /// The next-shorter style the compact layout derives when nothing pins
    /// it explicitly.
    pub fn compact_fallback(self) -> CwdStyle {
        match self {
            CwdStyle::Full => CwdStyle::Short,
            CwdStyle::Short | CwdStyle::Name => CwdStyle::Name,
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> String {
    format!("{}38;2;{};{};{}m", ESC, r, g, b)
}

fn bold(color: String) -> String {
    format!("\x1b[1m{color}")
}

/// The theme-dependent slots, mirroring the host's dark/light palettes
/// (tui/theme/colors.ts). Bar levels keep the host's diff/success hues;
/// everything the footer itself renders — mode badges, model, cwd, git —
/// uses the exact token values the host would have used.
pub struct Palette {
    pub text: String,
    pub text_dim: String,
    pub text_muted: String,
    pub warning: String,
    pub primary: String,
    pub accent: String,
    pub bar_red: String,
    pub bar_yellow: String,
    pub bar_green: String,
}

fn dark_palette() -> Palette {
    Palette {
        text: rgb(224, 224, 224),      // #E0E0E0 — model label
        text_dim: rgb(136, 136, 136),  // #888888 — cwd / git badge
        text_muted: rgb(107, 107, 107), // #6B6B6B — provisional readings
        warning: rgb(232, 168, 56),  // #E8A838 — auto/yolo badges (bold at call site)
        primary: rgb(79, 168, 255),  // #4FA8FF — plan badge
        accent: rgb(91, 192, 190),   // #5BC0BE — swarm badge
        bar_red: "\x1b[31m".to_string(),
        bar_yellow: "\x1b[33m".to_string(),
        bar_green: "\x1b[32m".to_string(),
    }
}

fn light_palette() -> Palette {
    Palette {
        text: rgb(26, 26, 26),         // #1A1A1A
        text_dim: rgb(69, 69, 69),     // #454545
        text_muted: rgb(95, 95, 95),   // #5F5F5F
        warning: rgb(146, 102, 10),  // #92660A
        primary: rgb(21, 101, 192),  // #1565C0
        accent: rgb(0, 131, 143),    // #00838F
        bar_red: rgb(185, 28, 28),   // #B91C1C — host light error
        bar_yellow: rgb(146, 102, 10), // #92660A — host light warning
        bar_green: rgb(14, 122, 56),  // #0E7A38 — host light success
    }
}

fn palette_for(theme: Theme) -> Palette {
    match theme {
        Theme::Dark => dark_palette(),
        Theme::Light => light_palette(),
    }
}

/// One slot's config as written in config.json's "slots" map. The flat
/// fields apply to both layouts; the nested "normal" / "compact" objects
/// override them for that layout, field by field.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SlotConfig {
    /// Theme token ("text" | "text_dim" | "text_muted" | "primary" |
    /// "warning" | "accent" | "default") or a hex value like "#FF8800".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    /// "long" | "short".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// speed only: show the TTFT reading (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normal: Option<SlotOverrides>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact: Option<SlotOverrides>,
}

/// Layout-specific overrides inside a SlotConfig; same fields, no nesting.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SlotOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// speed only: show the TTFT reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft: Option<bool>,
}

/// A theme token a style override can reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleToken {
    Text,
    TextDim,
    TextMuted,
    Primary,
    Warning,
    Accent,
    /// No color — plain terminal foreground.
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedColor {
    Token(StyleToken),
    Hex(u8, u8, u8),
}

/// Field-wise style for one slot; `None` fields fall through to the next
/// precedence level (styles_compact/styles_normal > styles > built-in).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SegmentStyle {
    pub color: Option<ResolvedColor>,
    pub bold: Option<bool>,
}

/// A slot's text format: the long form (normal layout's default) or the
/// short form (compact layout's default). Explicit config wins over the
/// layout default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentFormat {
    Long,
    Short,
}

pub fn parse_format(value: &str) -> Option<SegmentFormat> {
    match value {
        "long" => Some(SegmentFormat::Long),
        "short" => Some(SegmentFormat::Short),
        _ => None,
    }
}

fn parse_style_color(value: &str) -> Option<ResolvedColor> {
    let token = match value {
        "text" => StyleToken::Text,
        "text_dim" => StyleToken::TextDim,
        "text_muted" => StyleToken::TextMuted,
        "primary" => StyleToken::Primary,
        "warning" => StyleToken::Warning,
        "accent" => StyleToken::Accent,
        "default" => StyleToken::Default,
        _ => {
            let hex = value.strip_prefix('#')?;
            if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let byte = |r: std::ops::Range<usize>| u8::from_str_radix(&hex[r], 16).ok();
            return Some(ResolvedColor::Hex(byte(0..2)?, byte(2..4)?, byte(4..6)?));
        }
    };
    Some(ResolvedColor::Token(token))
}

fn token_sgr(palette: &Palette, token: StyleToken) -> String {
    match token {
        StyleToken::Text => palette.text.clone(),
        StyleToken::TextDim => palette.text_dim.clone(),
        StyleToken::TextMuted => palette.text_muted.clone(),
        StyleToken::Primary => palette.primary.clone(),
        StyleToken::Warning => palette.warning.clone(),
        StyleToken::Accent => palette.accent.clone(),
        StyleToken::Default => String::new(),
    }
}

/// Resolve the "slots" config for one layout into the style and format
/// maps the renderer consumes. Base fields first, then the nested
/// normal/compact object on top; invalid values drop (fail-soft). Entries
/// with no effective style field stay out of the style map so segments keep
/// their built-in coloring (threshold colors, stale muting).
pub fn resolve_slots(
    slots: Option<&std::collections::HashMap<String, SlotConfig>>,
    compact_layout: bool,
) -> ResolvedSlots {
    let mut styles = std::collections::HashMap::new();
    let mut formats = std::collections::HashMap::new();
    let mut cwd_normal = None;
    let mut cwd_compact = None;
    let mut show_ttft = true;
    let Some(slots) = slots else {
        return ResolvedSlots {
            styles,
            formats,
            cwd_normal: CwdStyle::Short,
            cwd_compact: CwdStyle::Name,
            show_ttft,
        };
    };
    for (slot, config) in slots {
        let mut style = SegmentStyle::default();
        let mut format: Option<SegmentFormat> = None;
        let mut apply = |color: Option<&str>, bold: Option<bool>, fmt: Option<&str>| {
            if let Some(color) = color.and_then(parse_style_color) {
                style.color = Some(color);
            }
            if bold.is_some() {
                style.bold = bold;
            }
            if let Some(parsed) = fmt.and_then(parse_format) {
                format = Some(parsed);
            }
        };
        apply(config.color.as_deref(), config.bold, config.format.as_deref());
        if slot == "cwd" {
            // The cwd form is read from both nested layers regardless of the
            // configured layout: nested normal wins over the flat value,
            // compact falls back to the next-shorter form of the effective
            // normal style unless its nested object pins one.
            cwd_normal = config
                .normal
                .as_ref()
                .and_then(|l| CwdStyle::parse(l.format.as_deref()))
                .or_else(|| CwdStyle::parse(config.format.as_deref()));
            cwd_compact = config
                .compact
                .as_ref()
                .and_then(|l| CwdStyle::parse(l.format.as_deref()));
        }
        if let Some(layer) = if compact_layout { config.compact.as_ref() } else { config.normal.as_ref() } {
            apply(layer.color.as_deref(), layer.bold, layer.format.as_deref());
            if slot == "speed" {
                if let Some(ttft) = layer.ttft {
                    show_ttft = ttft;
                }
            }
        }
        if slot == "speed" {
            if let Some(ttft) = config.ttft {
                show_ttft = ttft;
            }
        }
        if style.color.is_some() || style.bold.is_some() {
            styles.insert(slot.clone(), style);
        }
        // cwd's `format` is a CwdStyle (short/full/name), not long/short.
        if let Some(format) = format {
            if slot != "cwd" {
                formats.insert(slot.clone(), format);
            }
        }
    }
    let cwd_normal = cwd_normal.unwrap_or(CwdStyle::Short);
    ResolvedSlots {
        styles,
        formats,
        cwd_normal,
        cwd_compact: cwd_compact.unwrap_or_else(|| cwd_normal.compact_fallback()),
        show_ttft,
    }
}

/// Everything the renderer needs from the "slots" config section.
pub struct ResolvedSlots {
    pub styles: std::collections::HashMap<String, SegmentStyle>,
    pub formats: std::collections::HashMap<String, SegmentFormat>,
    /// cwd form per layout: flat `format` value, nested overrides applied,
    /// compact deriving the next-shorter style unless pinned.
    pub cwd_normal: CwdStyle,
    pub cwd_compact: CwdStyle,
    /// speed only: whether the TTFT reading may render.
    pub show_ttft: bool,
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
    pub git: GitSummary,
    pub items: &'a [String],
    pub cwd_style: CwdStyle,
    pub cwd_compact: CwdStyle,
    pub styles: &'a std::collections::HashMap<String, SegmentStyle>,
    pub formats: &'a std::collections::HashMap<String, SegmentFormat>,
    pub show_ttft: bool,
    pub layout: &'a str,
    pub color: bool,
    pub theme: Theme,
    pub now: u64,
}

/// SGR prefix for a slot with a user style override, or None when the slot
/// renders with its built-in default. `default_bold` is the slot's built-in
/// boldness, used when the override sets only a color.
fn style_sgr(style: Option<&SegmentStyle>, palette: &Palette, default_color: &str, default_bold: bool) -> String {
    let Some(style) = style else {
        return if default_bold { bold(default_color.to_string()) } else { default_color.to_string() };
    };
    let color = match style.color {
        Some(ResolvedColor::Token(token)) => token_sgr(palette, token),
        Some(ResolvedColor::Hex(r, g, b)) => rgb(r, g, b),
        None => String::new(),
    };
    if style.bold.unwrap_or(default_bold) {
        bold(color)
    } else {
        color
    }
}

/// Composed SGR for a slot: the override if configured, else the segment
/// default color and boldness.
fn slot_sgr(ctx: &RenderContext, palette: &Palette, slot: &str, default_color: &str, default_bold: bool) -> String {
    style_sgr(ctx.styles.get(slot), palette, default_color, default_bold)
}

/// Like slot_sgr but for one mode badge: a badge-specific override
/// ("auto"/"yolo"/"plan"/"swarm") wins over the generic "mode" one.
fn mode_badge_sgr(ctx: &RenderContext, palette: &Palette, badge: &str, default_color: &str) -> String {
    let style = ctx
        .styles
        .get(badge)
        .or_else(|| ctx.styles.get("mode"));
    style_sgr(style, palette, default_color, true)
}

/// The slot's effective format: explicit config wins, else the layout
/// default (compact = short, normal = long).
fn short_form(ctx: &RenderContext, slot: &str, compact: bool) -> bool {
    match ctx.formats.get(slot) {
        Some(SegmentFormat::Short) => true,
        Some(SegmentFormat::Long) => false,
        None => compact,
    }
}

/// Wrap a plain-text segment with its style override when one is configured;
/// None means no override and the caller keeps its built-in coloring.
fn styled(ctx: &RenderContext, palette: &Palette, slot: &str, text: &str, default_bold: bool) -> Option<String> {
    let style = ctx.styles.get(slot)?;
    if style.color.is_none() && style.bold.is_none() {
        return None;
    }
    Some(colorize(ctx.color, &style_sgr(Some(style), palette, "", default_bold), text))
}

/// The footer's mode slot: auto/yolo (warning), plan (primary), swarm
/// (accent) — all bold, bracket-free, single-space joined inside the slot.
/// Other permission modes render nothing, exactly like the host.
fn mode_slot(ctx: &RenderContext, palette: &Palette) -> Option<String> {
    let mut modes: Vec<String> = Vec::new();
    match ctx.payload.permission_mode.as_deref() {
        Some("auto") | Some("yolo") => {
            let mode = ctx.payload.permission_mode.as_deref().unwrap_or_default();
            modes.push(colorize(ctx.color, &mode_badge_sgr(ctx, palette, mode, &palette.warning), mode));
        }
        _ => {}
    }
    if ctx.payload.plan_mode.unwrap_or(false) {
        modes.push(colorize(ctx.color, &mode_badge_sgr(ctx, palette, "plan", &palette.primary), "plan"));
    }
    if ctx.metrics.swarm_mode || ctx.payload.swarm_mode.unwrap_or(false) {
        modes.push(colorize(ctx.color, &mode_badge_sgr(ctx, palette, "swarm", &palette.accent), "swarm"));
    }
    (!modes.is_empty()).then(|| modes.join(" "))
}

/// The footer's model slot: model display name plus the host's thinking
/// suffix (" thinking" / " thinking: <effort>", none when off), the whole
/// label in plain text color exactly like chalk.hex(colors.text)(label).
/// The footer's tasks slot: one badge per background BPM kind, zero hides
/// it. `[2 tasks running]  [1 agent running]` in primary (not bold),
/// two-space joined like the host composes sibling pieces.
fn tasks_slot(ctx: &RenderContext, palette: &Palette) -> Option<String> {
    let mut badges: Vec<String> = Vec::new();
    if ctx.metrics.bg_tasks > 0 {
        let noun = if ctx.metrics.bg_tasks == 1 { "task" } else { "tasks" };
        badges.push(format!("[{} {} running]", ctx.metrics.bg_tasks, noun));
    }
    if ctx.metrics.bg_agents > 0 {
        let noun = if ctx.metrics.bg_agents == 1 { "agent" } else { "agents" };
        badges.push(format!("[{} {} running]", ctx.metrics.bg_agents, noun));
    }
    let text = badges.join("  ");
    (!text.is_empty()).then(|| colorize(ctx.color, &slot_sgr(ctx, palette, "tasks", &palette.primary, false), &text))
}

fn model_segment(ctx: &RenderContext, palette: &Palette) -> Option<String> {
    let model = sanitize_terminal_text(ctx.payload.model.as_deref().unwrap_or(""));
    if model.is_empty() {
        return None;
    }
    let level = ctx
        .metrics
        .thinking_level
        .as_deref()
        .filter(|l| !l.is_empty() && *l != "off")
        .map(sanitize_terminal_text);
    let suffix = match level.as_deref() {
        None => String::new(),
        Some("on") => " thinking".to_string(),
        Some(level) => format!(" thinking: {}", level),
    };
    Some(colorize(ctx.color, &slot_sgr(ctx, palette, "model", &palette.text, false), &format!("{}{}", model, suffix)))
}

fn home_dir() -> Option<String> {
    std::env::var("HOME").ok().filter(|h| !h.is_empty())
}

/// `~`-abbreviate like the host's shortenCwd: exact home becomes `~`, a home
/// prefix becomes `~/…`.
fn abbreviate_home(path: &str, home: Option<&str>) -> String {
    if let Some(home) = home.filter(|h| !h.is_empty()) {
        if path == home {
            return "~".to_string();
        }
        if let Some(rest) = path.strip_prefix(home).filter(|r| r.starts_with('/')) {
            return format!("~{}", rest);
        }
    }
    path.to_string()
}

fn cwd_last_component(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// The footer's cwd slot (textDim): host-style `…/` abbreviation by default,
/// or the full path / last component per format; the compact layout gets the
/// next-shorter form unless pinned via slots.cwd.compact.format.
fn cwd_segment(ctx: &RenderContext, palette: &Palette, compact: bool) -> Option<String> {
    let cwd = sanitize_terminal_text(ctx.payload.cwd.as_deref().unwrap_or(""));
    if cwd.is_empty() {
        return None;
    }
    let style = if compact { ctx.cwd_compact } else { ctx.cwd_style };
    let home = home_dir();
    let text = match style {
        CwdStyle::Full => abbreviate_home(&cwd, home.as_deref()),
        CwdStyle::Name => {
            if home.as_deref() == Some(cwd.as_str()) {
                "~".to_string()
            } else {
                cwd_last_component(&cwd)
            }
        }
        CwdStyle::Short => {
            let work = abbreviate_home(&cwd, home.as_deref());
            let segments: Vec<&str> = work.split('/').filter(|s| !s.is_empty()).collect();
            if segments.len() <= 3 {
                work
            } else {
                format!("…/{}", segments[segments.len() - 3..].join("/"))
            }
        }
    };
    Some(colorize(ctx.color, &slot_sgr(ctx, palette, "cwd", &palette.text_dim, false), &text))
}

/// The footer's git badge (textDim), mirroring formatGitBadgeBase:
/// `main` / `main [±]` / `main [+3 -1 ↑2 ↓1]`. The branch comes from the
/// host payload when present, else from our own probe.
fn git_segment(ctx: &RenderContext, palette: &Palette, short: bool) -> Option<String> {
    let branch = ctx
        .payload
        .git_branch
        .clone()
        .or_else(|| ctx.git.branch.clone())
        .map(|b| sanitize_terminal_text(&b))
        .filter(|b| !b.is_empty());
    let branch = branch?;
    let mut parts: Vec<String> = Vec::new();
    if ctx.git.diff_added > 0 || ctx.git.diff_deleted > 0 {
        let mut diff: Vec<String> = Vec::new();
        if ctx.git.diff_added > 0 {
            diff.push(format!("+{}", ctx.git.diff_added));
        }
        if ctx.git.diff_deleted > 0 {
            diff.push(format!("-{}", ctx.git.diff_deleted));
        }
        parts.push(diff.join(" "));
    } else if ctx.git.dirty {
        parts.push("±".to_string());
    }
    let mut sync = String::new();
    if ctx.git.ahead > 0 {
        sync.push_str(&format!("↑{}", ctx.git.ahead));
    }
    if ctx.git.behind > 0 {
        sync.push_str(&format!("↓{}", ctx.git.behind));
    }
    if !sync.is_empty() {
        parts.push(sync);
    }
    let text = if short {
        if ctx.git.dirty {
            format!("{}*", branch)
        } else {
            branch
        }
    } else if parts.is_empty() {
        branch
    } else {
        format!("{} [{}]", branch, parts.join(" "))
    };
    Some(colorize(ctx.color, &slot_sgr(ctx, palette, "git", &palette.text_dim, false), &text))
}

fn speed_segment(ctx: &RenderContext, palette: &Palette, short: bool) -> Option<String> {
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

    let speed_styled = ctx.styles.contains_key("speed");
    if let Some(tps) = metrics.tps {
        let average = tps.round();
        let paint = |text: &str| -> String {
            if metrics.tps_stale && !speed_styled {
                colorize(ctx.color, &palette.text_muted, text)
            } else {
                text.to_string()
            }
        };
        if short {
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
            let muted = if speed_styled { format!(" · compacted {}", c) } else { colorize(ctx.color, &palette.text_muted, &format!(" · compacted {}", c)) };
        return Some(format!("{}{}", paint(&base), muted));
        }
        if !ctx.show_ttft {
            return Some(paint(&base));
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
        if !short {
            let text = format!("compacted {}", c);
            return Some(if speed_styled { text } else { colorize(ctx.color, &palette.text_muted, &text) });
        }
    }
    if !ctx.show_ttft {
        return None;
    }
    format_ttft(metrics.ttft_ms).map(|ttft| format!("TTFT {}", ttft))
}

fn cache_segment(ctx: &RenderContext, short: bool) -> Option<String> {
    let hit_rate = ctx.metrics.cache.as_ref()?.hit_rate;
    if !hit_rate.is_finite() || !(0.0..=1.0).contains(&hit_rate) {
        return None;
    }
    let label = if short { "C" } else { "Cache" };
    Some(format!("{} {}%", label, (hit_rate * 100.0).round() as i64))
}

fn quota_segment(ctx: &RenderContext, palette: &Palette, short: bool) -> Option<String> {
    let quota = ctx.quota?;
    let quota_styled = ctx.styles.contains_key("quota");
    let mut parts: Vec<String> = Vec::new();
    for window in &quota.windows {
        let fraction = window.used / window.limit;
        let pct = format!("{}%", pct_of(window.used, window.limit));
        let label = sanitize_terminal_text(&window.label);
        let mut text = if quota_styled {
            format!("{} {}", label, pct)
        } else if short {
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
    if let Some(weekly) = &quota.weekly {
        let fraction = weekly.used / weekly.limit;
        let pct = format!("{}%", pct_of(weekly.used, weekly.limit));
        let mut text = if quota_styled {
            format!("7d {pct}")
        } else if short {
            match number_level_color(fraction, palette) {
                Some(level) => format!("7d {}", colorize(ctx.color, &level, &pct)),
                None => format!("7d {pct}"),
            }
        } else {
            format!("7d {} {}", bar(fraction, ctx.color, palette), pct)
        };
        if let Some(countdown) = format_countdown(weekly.reset_at.as_deref(), ctx.now) {
            text.push_str(&format!(" {}", countdown));
        }
        parts.push(text);
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// One configured slot by name; unknown names are skipped like the host's
/// own items handling.
fn slot_segment(ctx: &RenderContext, palette: &Palette, name: &str, compact: bool) -> Option<String> {
    match name {
        "mode" => mode_slot(ctx, palette),
        "model" => model_segment(ctx, palette),
        "tasks" => tasks_slot(ctx, palette),
        "cwd" => cwd_segment(ctx, palette, compact),
        "git" => git_segment(ctx, palette, short_form(ctx, "git", compact)),
        "speed" => speed_segment(ctx, palette, short_form(ctx, "speed", compact))
            .map(|s| styled(ctx, palette, "speed", &s, false).unwrap_or(s)),
        "cache" => cache_segment(ctx, short_form(ctx, "cache", compact))
            .map(|s| styled(ctx, palette, "cache", &s, false).unwrap_or(s)),
        "quota" => quota_segment(ctx, palette, short_form(ctx, "quota", compact))
            .map(|s| styled(ctx, palette, "quota", &s, false).unwrap_or(s)),
        _ => None,
    }
}

/// Render the HUD line: the configured slots joined with the footer's
/// two-space separator. Downgrades normal -> compact above 200 visible
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
        let mut slots: Vec<String> = Vec::new();
        for name in ctx.items {
            if let Some(segment) = slot_segment(ctx, &palette, name, compact) {
                if !segment.is_empty() {
                    slots.push(segment);
                }
            }
        }
        let line = slots.join("  ");
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

    fn default_items() -> Vec<String> {
        DEFAULT_ITEMS.iter().map(|s| s.to_string()).collect()
    }

    fn empty_styles() -> &'static std::collections::HashMap<String, SegmentStyle> {
        Box::leak(Box::new(std::collections::HashMap::new()))
    }

    fn empty_formats() -> &'static std::collections::HashMap<String, SegmentFormat> {
        Box::leak(Box::new(std::collections::HashMap::new()))
    }

    fn ctx<'a>(
        payload: &'a Payload,
        metrics_value: &'a MetricsSummary,
        quota: Option<&'a QuotaCache>,
    ) -> RenderContext<'a> {
        let items: &'a Vec<String> = Box::leak(Box::new(default_items()));
        RenderContext {
            payload,
            quota,
            metrics: metrics_value,
            git: GitSummary::default(),
            items,
            cwd_style: CwdStyle::Short,
            cwd_compact: CwdStyle::Name,
            styles: empty_styles(),
            formats: empty_formats(),
            show_ttft: true,
            layout: "normal",
            color: false,
            theme: Theme::Dark,
            now: 1_800_000_000_000,
        }
    }

    fn styled_ctx<'a>(
        payload: &'a Payload,
        metrics_value: &'a MetricsSummary,
        quota: Option<&'a QuotaCache>,
        styles: &'a std::collections::HashMap<String, SegmentStyle>,
    ) -> RenderContext<'a> {
        RenderContext {
            styles,
            ..ctx(payload, metrics_value, quota)
        }
    }

    fn formats_ctx<'a>(
        payload: &'a Payload,
        metrics_value: &'a MetricsSummary,
        quota: Option<&'a QuotaCache>,
        formats: &'a std::collections::HashMap<String, SegmentFormat>,
        layout: &'a str,
    ) -> RenderContext<'a> {
        RenderContext {
            formats,
            layout,
            ..ctx(payload, metrics_value, quota)
        }
    }

    #[test]
    fn format_overrides_beat_layout_defaults() {
        let mut payload = base_payload();
        payload.permission_mode = Some("yolo".to_string());
        let mut m = metrics();
        m.tps = Some(46.7);
        m.cache = Some(crate::metrics::CacheMetric { hit_rate: 0.92 });
        let mut formats = std::collections::HashMap::new();
        formats.insert("git".to_string(), SegmentFormat::Short);
        formats.insert("cache".to_string(), SegmentFormat::Short);
        formats.insert("speed".to_string(), SegmentFormat::Long);

        let dirty = GitSummary { dirty: true, ..GitSummary::default() };

        // Normal layout with short git/cache forced: main* and C 92%.
        let ctx = RenderContext { git: dirty.clone(), ..formats_ctx(&payload, &m, None, &formats, "normal") };
        let line = render_hud(&ctx);
        assert!(line.contains("main*"), "line was: {line}");
        assert!(line.contains("C 92%"), "line was: {line}");
        assert!(line.contains("⚡ 47 t/s"), "long speed forced: {line}");

        // Compact layout with long speed forced keeps the long reading.
        let ctx = RenderContext { git: dirty, ..formats_ctx(&payload, &m, None, &formats, "compact") };
        let line = render_hud(&ctx);
        assert!(line.contains("⚡ 47 t/s"), "line was: {line}");

        // No overrides: compact defaults shorten everything.
        let empty = std::collections::HashMap::new();
        let ctx = RenderContext {
            git: GitSummary { dirty: true, ..GitSummary::default() },
            ..formats_ctx(&payload, &m, None, &empty, "compact")
        };
        let line = render_hud(&ctx);
        assert!(line.contains("main*"), "line was: {line}");
        assert!(line.contains("⚡ 47 "), "line was: {line}");
        assert!(!line.contains("t/s"), "line was: {line}");
        assert!(line.contains("C 92%"), "line was: {line}");
    }

    #[test]
    fn style_overrides_recolor_slots() {
        let payload = base_payload();
        let m = metrics();
        let mut styles = std::collections::HashMap::new();
        styles.insert(
            "git".to_string(),
            SegmentStyle {
                color: Some(ResolvedColor::Hex(255, 136, 0)),
                bold: Some(true),
            },
        );
        styles.insert(
            "model".to_string(),
            SegmentStyle { color: Some(ResolvedColor::Token(StyleToken::TextMuted)), bold: None },
        );
        let ctx = RenderContext { color: true, ..styled_ctx(&payload, &m, None, &styles) };
        let line = render_hud(&ctx);
        let palette = dark_palette();
        let expected_git = "\x1b[1m\x1b[38;2;255;136;0mmain\x1b[0m";
        assert!(line.contains(&expected_git), "line was: {line:?}");
        // Token override: model painted with the palette's text_muted SGR.
        assert!(line.contains(&format!("{}K3\x1b[0m", palette.text_muted)), "line was: {line:?}");
        let _ = palette;
    }

    #[test]
    fn speed_style_wraps_whole_segment_even_when_stale() {
        let payload = base_payload();
        let mut m = metrics();
        m.tps = Some(46.7);
        m.tps_stale = true;
        let mut styles = std::collections::HashMap::new();
        styles.insert(
            "speed".to_string(),
            SegmentStyle { color: Some(ResolvedColor::Token(StyleToken::Accent)), bold: Some(true) },
        );
        let ctx = RenderContext { color: true, ..styled_ctx(&payload, &m, None, &styles) };
        let line = render_hud(&ctx);
        let palette = dark_palette();
        // One wrap around the whole reading; no stale-muting inside.
        assert!(!line.contains(&palette.text_muted), "line was: {line:?}");
        assert!(line.contains(&format!("\x1b[1m{}⚡ 47 t/s", palette.accent)), "line was: {line:?}");
    }

    #[test]
    fn resolve_slots_layers_and_fail_soft() {
        use std::collections::HashMap;
        let mut slots: HashMap<String, SlotConfig> = HashMap::new();
        slots.insert(
            "git".to_string(),
            SlotConfig {
                color: Some("#888888".to_string()),
                bold: Some(false),
                normal: Some(SlotOverrides {
                    color: Some("primary".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        // Invalid color drops; valid bold sticks.
        slots.insert(
            "cwd".to_string(),
            SlotConfig { color: Some("nope".to_string()), bold: Some(true), ..Default::default() },
        );
        // Format-only entries land in formats, not styles.
        slots.insert(
            "speed".to_string(),
            SlotConfig {
                compact: Some(SlotOverrides { format: Some("long".to_string()), ..Default::default() }),
                ..Default::default()
            },
        );
        let normal = resolve_slots(Some(&slots), false);
        let styles_normal = &normal.styles;
        let _formats_normal = &normal.formats;
        // Nested normal layer wins per-field; unspecified bold survives.
        assert_eq!(
            styles_normal.get("git"),
            Some(&SegmentStyle {
                color: Some(ResolvedColor::Token(StyleToken::Primary)),
                bold: Some(false),
            })
        );
        assert_eq!(
            styles_normal.get("cwd"),
            Some(&SegmentStyle { color: None, bold: Some(true) })
        );
        assert!(!styles_normal.contains_key("speed"));
        // Compact: speed forced long; git keeps the base hex color.
        let compact = resolve_slots(Some(&slots), true);
        let styles_compact = &compact.styles;
        let formats_compact = &compact.formats;
        assert_eq!(formats_compact.get("speed"), Some(&SegmentFormat::Long));
        assert_eq!(
            styles_compact.get("git"),
            Some(&SegmentStyle {
                color: Some(ResolvedColor::Hex(136, 136, 136)),
                bold: Some(false),
            })
        );
    }

    #[test]
    fn background_task_badges_render() {
        let payload = base_payload();
        let mut m = metrics();
        m.bg_tasks = 2;
        let two = ctx(&payload, &m, None);
        let line = render_hud(&two);
        assert!(line.contains("[2 tasks running]"), "line was: {line}");
        m.bg_agents = 1;
        let both = ctx(&payload, &m, None);
        let line = render_hud(&both);
        assert!(line.contains("[2 tasks running]  [1 agent running]"), "line was: {line}");
        // Zero on both sides hides the slot entirely.
        m.bg_tasks = 0;
        m.bg_agents = 0;
        let none = ctx(&payload, &m, None);
        assert!(!render_hud(&none).contains("running"));
    }

    #[test]
    fn ttft_flag_hides_the_reading_but_keeps_tps() {
        let payload = base_payload();
        let mut m = metrics();
        m.tps = Some(46.7);
        m.ttft_ms = Some(1_300.0);
        // Hidden: the t/s reading stays, TTFT goes; the bare fallback
        // (no TPS at all) renders nothing.
        let with_tps = RenderContext { show_ttft: false, ..ctx(&payload, &m, None) };
        let line = render_hud(&with_tps);
        assert!(line.contains("⚡ 47 t/s"), "line was: {line}");
        assert!(!line.contains("TTFT"), "line was: {line}");
        let mut m2 = metrics();
        m2.ttft_ms = Some(1_300.0);
        let bare = RenderContext { show_ttft: false, ..ctx(&payload, &m2, None) };
        assert!(!render_hud(&bare).contains("TTFT"));
    }

    #[test]
    fn resolve_slots_ttft_flag() {
        use std::collections::HashMap;
        // Flat false hides in both layouts.
        let mut slots: HashMap<String, SlotConfig> = HashMap::new();
        slots.insert(
            "speed".to_string(),
            SlotConfig { ttft: Some(false), ..Default::default() },
        );
        assert!(!resolve_slots(Some(&slots), false).show_ttft);
        assert!(!resolve_slots(Some(&slots), true).show_ttft);
        // Nested compact-only hiding: normal keeps it.
        let mut slots: HashMap<String, SlotConfig> = HashMap::new();
        slots.insert(
            "speed".to_string(),
            SlotConfig {
                compact: Some(SlotOverrides { ttft: Some(false), ..Default::default() }),
                ..Default::default()
            },
        );
        assert!(resolve_slots(Some(&slots), false).show_ttft);
        assert!(!resolve_slots(Some(&slots), true).show_ttft);
    }

    #[test]
    fn resolve_slots_cwd_format_and_derivation() {
        use std::collections::HashMap;
        let mut slots: HashMap<String, SlotConfig> = HashMap::new();
        // Flat full: normal full, compact derives short. "long" aliases full.
        slots.insert(
            "cwd".to_string(),
            SlotConfig { format: Some("long".to_string()), ..Default::default() },
        );
        let resolved = resolve_slots(Some(&slots), false);
        assert_eq!(resolved.cwd_normal, CwdStyle::Full);
        assert_eq!(resolved.cwd_compact, CwdStyle::Short);
        // Nested compact pins full even in the compact layout.
        let mut slots: HashMap<String, SlotConfig> = HashMap::new();
        slots.insert(
            "cwd".to_string(),
            SlotConfig {
                format: Some("short".to_string()),
                compact: Some(SlotOverrides {
                    format: Some("full".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let resolved = resolve_slots(Some(&slots), true);
        assert_eq!(resolved.cwd_normal, CwdStyle::Short);
        assert_eq!(resolved.cwd_compact, CwdStyle::Full);
        // No cwd entry: built-in defaults.
        let resolved = resolve_slots(None, false);
        assert_eq!(resolved.cwd_normal, CwdStyle::Short);
        assert_eq!(resolved.cwd_compact, CwdStyle::Name);
    }

    #[test]
    fn plain_line_without_metrics() {
        let payload = base_payload();
        let m = metrics();
        let line = render_hud(&ctx(&payload, &m, None));
        // Manual mode renders no badge, exactly like the host footer.
        assert!(!line.contains("manual"));
        assert!(line.contains("K3"));
        // cwd short style keeps ≤3 segments intact; git badge is bare branch.
        assert!(line.contains("work/kimi-code-hud"));
        assert!(line.contains("main"));
        assert!(!line.contains("git:("));
        assert!(!line.contains("±"));
        assert!(!line.contains("Cache"));
        assert!(!line.contains("5h"));
    }

    #[test]
    fn mode_slot_matches_host_footer() {
        let mut payload = base_payload();
        payload.permission_mode = Some("auto".to_string());
        payload.plan_mode = Some(true);
        let m = metrics();
        let items = vec!["mode".to_string()];
        let rendered = RenderContext {
            items: &items,
            ..ctx(&payload, &m, None)
        };
        assert_eq!(render_hud(&rendered), "auto plan");

        let mut payload = base_payload();
        payload.permission_mode = Some("yolo".to_string());
        let rendered = RenderContext {
            items: &items,
            ..ctx(&payload, &m, None)
        };
        assert_eq!(render_hud(&rendered), "yolo");
    }

    #[test]
    fn thinking_suffix_matches_host_footer() {
        let mut payload = base_payload();
        payload.model = Some("kimi-k2".to_string());
        let items = vec!["model".to_string()];
        let mut m = metrics();
        m.thinking_level = Some("high".to_string());
        assert_eq!(
            render_hud(&RenderContext { items: &items, ..ctx(&payload, &m, None) }),
            "kimi-k2 thinking: high"
        );

        m.thinking_level = Some("on".to_string());
        assert_eq!(
            render_hud(&RenderContext { items: &items, ..ctx(&payload, &m, None) }),
            "kimi-k2 thinking"
        );

        m.thinking_level = Some("off".to_string());
        assert_eq!(
            render_hud(&RenderContext { items: &items, ..ctx(&payload, &m, None) }),
            "kimi-k2"
        );
    }

    #[test]
    fn cwd_styles_short_full_name() {
        let mut payload = base_payload();
        // Machine-independent path: never under HOME, 5 segments long.
        payload.cwd = Some("/opt/dev/开发/RustProjects/kimi-code-hud-rs".to_string());
        let m = metrics();
        let items = vec!["cwd".to_string()];
        let rendered = RenderContext {
            items: &items,
            cwd_style: CwdStyle::Name,
            ..ctx(&payload, &m, None)
        };
        assert_eq!(render_hud(&rendered), "kimi-code-hud-rs");

        let rendered = RenderContext {
            cwd_style: CwdStyle::Full,
            ..rendered
        };
        assert_eq!(render_hud(&rendered), "/opt/dev/开发/RustProjects/kimi-code-hud-rs");

        let rendered = RenderContext {
            cwd_style: CwdStyle::Short,
            ..rendered
        };
        // 5 segments shorten to the trailing 3 with the host's …/ prefix;
        // ≤3-segment paths keep the original leading slash, like the host.
        assert_eq!(render_hud(&rendered), "…/开发/RustProjects/kimi-code-hud-rs");
    }

    #[test]
    fn git_badge_matches_host_footer() {
        let payload = base_payload();
        let m = metrics();
        let items = vec!["git".to_string()];
        let dirty_no_counts = GitSummary {
            branch: None,
            dirty: true,
            ..GitSummary::default()
        };
        let rendered = RenderContext {
            items: &items,
            git: dirty_no_counts,
            ..ctx(&payload, &m, None)
        };
        assert_eq!(render_hud(&rendered), "main [±]");

        let counted = GitSummary {
            dirty: true,
            diff_added: 3,
            diff_deleted: 1,
            ahead: 2,
            behind: 1,
            ..GitSummary::default()
        };
        let rendered = RenderContext {
            git: counted,
            ..rendered
        };
        assert_eq!(render_hud(&rendered), "main [+3 -1 ↑2↓1]");

        let ahead_only = GitSummary {
            ahead: 1,
            ..GitSummary::default()
        };
        let rendered = RenderContext {
            git: ahead_only,
            ..rendered
        };
        assert_eq!(render_hud(&rendered), "main [↑1]");

        // No payload branch: fall back to the probe's own parse.
        let mut payload = base_payload();
        payload.git_branch = None;
        let probed = GitSummary {
            branch: Some("dev".to_string()),
            ..GitSummary::default()
        };
        let rendered = RenderContext {
            payload: &payload,
            git: probed,
            ..rendered
        };
        assert_eq!(render_hud(&rendered), "dev");
    }

    #[test]
    fn items_order_is_honored_and_unknown_skipped() {
        let payload = base_payload();
        let m = metrics();
        let items: Vec<String> = ["git", "bogus", "model", "cwd"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let rendered = RenderContext {
            items: &items,
            ..ctx(&payload, &m, None)
        };
        let line = render_hud(&rendered);
        assert_eq!(line, "main  K3  /work/kimi-code-hud");
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

        // Compact keeps both windows, dropping only the bars.
        let compact = RenderContext {
            layout: "compact",
            ..ctx(&payload, &m, Some(&quota))
        };
        let line = render_hud(&compact);
        assert!(line.contains("5h 31%"), "line was: {line}");
        assert!(line.contains("7d 25%"), "line was: {line}");
        assert!(!line.contains("░"));
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

