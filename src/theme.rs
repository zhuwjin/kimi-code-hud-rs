// Theme resolution for the badge palette. Only the truecolor slots follow the
// theme; ANSI colors are remapped by the terminal itself.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

/// Background ANSI 16-color indices that read as dark: 0-6 and 8.
fn theme_from_colorfgbg(value: Option<&str>) -> Option<Theme> {
    let value = value?;
    let bg = value.split(';').next_back()?.trim().parse::<i64>().ok()?;
    if bg < 0 {
        return None;
    }
    Some(if (0..=6).contains(&bg) || bg == 8 {
        Theme::Dark
    } else {
        Theme::Light
    })
}

/// Top-level `theme = "..."` from tui.toml; only keys before the first
/// [section] header count.
pub fn theme_from_tui_toml(content: &str) -> Option<String> {
    for line in content.replace("\r\n", "\n").split('\n') {
        if line.trim_start().starts_with('[') {
            break;
        }
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("theme") {
            let rest = rest.trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                let value = value.trim();
                if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
                    || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
                {
                    return Some(value[1..value.len() - 1].to_string());
                }
            }
        }
    }
    None
}

/// Resolve the effective theme:
/// 1. KIMI_HUD_RS_THEME=dark|light — explicit override
/// 2. tui.toml top-level theme = "dark"|"light"
/// 3. "auto" / missing: COLORFGBG, then dark (a status-line command owns
///    neither stdin nor stdout, so the host's OSC 11 probe is unavailable).
pub fn resolve_theme(env_kimi_hud_theme: Option<&str>, tui_toml_text: &str, env_colorfgbg: Option<&str>) -> Theme {
    if let Some(value) = env_kimi_hud_theme {
        if value == "dark" {
            return Theme::Dark;
        }
        if value == "light" {
            return Theme::Light;
        }
    }
    match theme_from_tui_toml(tui_toml_text).as_deref() {
        Some("dark") => Theme::Dark,
        Some("light") => Theme::Light,
        None | Some("auto") => theme_from_colorfgbg(env_colorfgbg).unwrap_or(Theme::Dark),
        Some(_) => Theme::Dark, // custom theme name — palette unknown, keep dark
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_override_then_toml_then_colorfgbg() {
        assert_eq!(resolve_theme(Some("light"), "theme = \"dark\"", None), Theme::Light);
        assert_eq!(resolve_theme(None, "theme = \"light\"\n[x]\ny = 1\n", None), Theme::Light);
        // theme key inside a section does not count.
        assert_eq!(resolve_theme(None, "[a]\ntheme = \"light\"\n", None), Theme::Dark);
        assert_eq!(resolve_theme(None, "theme = \"auto\"", Some("15;0")), Theme::Dark);
        assert_eq!(resolve_theme(None, "theme = \"auto\"", Some("0;15")), Theme::Light);
        assert_eq!(resolve_theme(None, "", None), Theme::Dark);
    }
}
