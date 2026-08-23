// Control plane: --install / --uninstall. --install registers the status
// line in tui.toml; the host rewrites that file on some upgrades (wiping
// [status_line]), so the user simply re-runs --install afterwards. Never
// touches a [status_line] command that is not ours — the user's own status
// line wins.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::toml_edit::{remove_status_line_command, set_status_line_command};
use crate::util::{atomic_write, read_string};
use crate::RuntimePaths;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HudConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// Status-line slot order, e.g. ["mode","model","cwd","git","speed","cache","quota"].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<String>>,
    /// Per-slot config: style and format overrides, keyed by slot name
    /// (the mode badges style individually as "auto"/"yolo"/"plan"/"swarm").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots: Option<std::collections::HashMap<String, crate::render::SlotConfig>>,
}

fn needs_quoting(path: &str) -> bool {
    path.is_empty()
        || !path.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '\\' | '-')
        })
}

/// Quote the command only when the shell could split or expand it, mirroring
/// the Node original's quoteCommandArg: paths of safe characters stay bare,
/// anything else is wrapped in double quotes with shell-significant
/// characters escaped.
fn quote_and_escape(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len() + 2);
    for c in path.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    format!("\"{}\"", escaped)
}

/// On Windows, prefer the 8.3 short path for spaced directories: it keeps
/// the stored command a bare single word (e.g. D:\PROGRA~1\jin\bin\...),
/// which every executor — shell or not — can run. Returns None when the
/// volume has 8dot3 names disabled or the short form still carries spaces.
#[cfg(windows)]
fn windows_short_path(path: &str) -> Option<String> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetShortPathNameW(filename: *const u16, short_path: *mut u16, buffer_len: u32) -> u32;
    }

    let wide: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();
    let needed = unsafe { GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0) };
    if needed == 0 {
        return None; // path missing or inaccessible
    }
    let mut buf = vec![0u16; needed as usize];
    let written = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if written == 0 || written as usize >= buf.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..written as usize]))
}

/// The executable as a command word: bare when possible, short-path bare on
/// Windows before falling back to quoting.
fn exe_command_word(exe: &Path) -> String {
    let path = exe.to_string_lossy().into_owned();
    if !needs_quoting(&path) {
        return path;
    }
    #[cfg(windows)]
    {
        if let Some(short) = windows_short_path(&path) {
            if !needs_quoting(&short) {
                return short;
            }
        }
    }
    quote_and_escape(&path)
}

/// Decide which executable the installed command should reference: the given
/// one when its path is already a bare command word, the Windows 8.3 short
/// form when the volume provides one, and otherwise a fresh copy under
/// <hud_dir>/bin — a space-free path this tool owns, so the stored command
/// never needs quoting regardless of how the host executes it. Returns the
/// exe to reference and whether a copy was made.
fn installable_exe(exe: &Path, hud_dir: &Path) -> (PathBuf, bool) {
    let path = exe.to_string_lossy().into_owned();
    if !needs_quoting(&path) {
        return (exe.to_path_buf(), false);
    }
    #[cfg(windows)]
    {
        if let Some(short) = windows_short_path(&path) {
            if !needs_quoting(&short) {
                return (PathBuf::from(short), false);
            }
        }
    }
    let dest = hud_dir.join("bin").join("kimi-code-hud-rs.exe");
    let dir = dest.parent().unwrap_or(hud_dir);
    if fs::create_dir_all(dir).is_ok()
        && fs::copy(exe, &dest).is_ok()
        && !needs_quoting(&dest.to_string_lossy())
    {
        return (dest, true);
    }
    (exe.to_path_buf(), false) // copy impossible — fall back to the quoted original
}

pub fn status_line_command(exe: &Path) -> String {
    exe_command_word(exe)
}

/// The default config file, written as JSONC so every option is documented
/// inline. `styles` keys are the slot names from `items`.
const DEFAULT_CONFIG_JSONC: &str = r##"{
  // Layout: "normal" | "compact" — compact is also the fallback when the
  // rendered line exceeds 200 visible characters.
  "layout": "normal",

  // Slot order; unknown names are ignored.
  "items": ["mode", "model", "tasks", "cwd", "git", "speed", "cache", "quota"],

  // Per-slot overrides, keyed by slot name. Flat fields apply to both
  // layouts; the nested "normal" / "compact" objects override them for
  // that layout, field by field. "color": theme token (text / text_dim /
  // text_muted / primary / warning / accent / default) or "#RRGGBB";
  // "bold": true | false; "format" per slot — long | short for
  // git/speed/cache/quota (short forms: git "main*", speed "⚡ 47", cache
  // "C 92%", quota without bars), short | full | name for cwd.
  //
  // The values below ARE the built-in defaults — edit in place. Note:
  // speed / cache / quota colors are deliberately unset; setting one
  // colors the whole segment and replaces the built-in threshold colors
  // and stale muting. The mode badges style individually.
  "slots": {
    "auto":  { "color": "warning", "bold": true },
    "yolo":  { "color": "warning", "bold": true },
    "plan":  { "color": "primary", "bold": true },
    "swarm": { "color": "accent",  "bold": true },

    "model": { "color": "text" },
    // Background task badges ("[2 tasks running] [1 agent running]"),
    // shown only while nonzero.
    "tasks": { "color": "primary" },
    // cwd format: "short" (host-like abbreviation), "full" (~-abbreviated
    // full path; "long" aliases it) or "name" (last component).
    "cwd":   {
      "color": "text_dim",
      "normal":  { "format": "short" },
      "compact": { "format": "name" }
    },
    "git":   {
      "color": "text_dim",
      "normal":  { "format": "long" },
      "compact": { "format": "short" }
    },
    "speed": {
      // "ttft": false hides the TTFT reading (default true).
      "normal":  { "format": "long" },
      "compact": { "format": "short" }
    },
    "cache": {
      "normal":  { "format": "long" },
      "compact": { "format": "short" }
    },
    "quota": {
      "normal":  { "format": "long" },
      "compact": { "format": "short" }
    }
  }
}
"##;

/// Create config.json with documented defaults when it does not exist yet,
/// so the file is discoverable right after --install. The file is parsed as
/// JSONC (comments and trailing commas allowed). An existing file is never
/// touched — even an unparseable one may hold user edits.
fn ensure_default_config(config_path: &Path) -> bool {
    if config_path.exists() {
        return false;
    }
    atomic_write(config_path, DEFAULT_CONFIG_JSONC.as_bytes()).is_ok()
}

fn backup_file(path: &Path) {
    // Best effort: the timestamped copy is a convenience, the atomic target
    // write below remains authoritative.
    let Ok(meta) = fs::metadata(path) else { return };
    if !meta.is_file() {
        return;
    }
    let stamp = crate::util::now_ms();
    let _ = fs::copy(path, backup_path(path, stamp));
}

fn backup_path(path: &Path, stamp: u64) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    path.with_file_name(format!("{}.{}.bak", name, stamp))
}

fn write_if_changed(path: &Path, current: &str, next: String) -> bool {
    if next == current.replace("\r\n", "\n") || next == current {
        return false;
    }
    backup_file(path);
    let _ = atomic_write(path, next.as_bytes());
    true
}

fn read_or_empty(path: &Path) -> String {
    read_string(path).unwrap_or_default()
}

fn install_status_line(paths: &RuntimePaths, command: &str) -> bool {
    let content = read_or_empty(&paths.tui_toml_path);
    let next = set_status_line_command(&content, command);
    write_if_changed(&paths.tui_toml_path, &content, next)
}

fn remove_status_line(paths: &RuntimePaths, command: &str) -> bool {
    let content = read_or_empty(&paths.tui_toml_path);
    let next = remove_status_line_command(&content, command);
    if next == content {
        return false;
    }
    backup_file(&paths.tui_toml_path);
    let _ = atomic_write(&paths.tui_toml_path, next.as_bytes());
    true
}

/// --install: register the status line in tui.toml. The host may wipe the
/// entry on upgrades — re-run --install to restore it.
pub fn install(exe: &Path, paths: &RuntimePaths) -> Result<(), String> {
    let (exe, copied) = installable_exe(exe, &paths.hud_dir);
    if copied {
        println!("Copied executable to {} (the build path contains spaces)", exe.display());
        println!("Re-run --install after each rebuild to refresh the copy.");
    }
    let command = status_line_command(&exe);
    if install_status_line(paths, &command) {
        println!("Registered status line in {}", paths.tui_toml_path.display());
    }
    if ensure_default_config(&paths.hud_config_path) {
        println!("Created default config at {}", paths.hud_config_path.display());
    }
    Ok(())
}

/// --uninstall: remove the status line, with a backup.
pub fn uninstall(exe: &Path, paths: &RuntimePaths) -> Result<(), String> {
    let command = status_line_command(exe);
    if remove_status_line(paths, &command) {
        println!("Removed status line from {}", paths.tui_toml_path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toml_edit::{get_status_line_command, is_own_command, set_status_line_command};

    #[test]
    fn default_config_created_once_and_never_clobbered() {
        let dir = std::env::temp_dir().join(format!("kimi-hud-rs-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        assert!(ensure_default_config(&path));
        let created = fs::read_to_string(&path).unwrap();
        assert!(created.contains("\"layout\": \"normal\""));

        assert!(created.contains("\"items\""));
        // Documented JSONC: comments inline, and parseable after stripping.
        assert!(created.contains("//"));
        let parsed: HudConfig =
            serde_json::from_str(&crate::util::strip_jsonc(&created)).unwrap();
        assert_eq!(parsed.layout.as_deref(), Some("normal"));
        let expected: Vec<String> = crate::render::DEFAULT_ITEMS.iter().map(|s| s.to_string()).collect();
        assert_eq!(parsed.items, Some(expected));
        // Full schema present: the slots map is pre-filled with the
        // built-in defaults and resolves to identical output.
        let slots = parsed.slots.expect("slots pre-filled");
        assert_eq!(slots.len(), 11);
        let resolved_normal = crate::render::resolve_slots(Some(&slots), false);
        let resolved_compact = crate::render::resolve_slots(Some(&slots), true);
        // cwd: text_dim token, short form; no long/short format; compact derives name.
        assert_eq!(
            resolved_normal.styles.get("cwd"),
            Some(&crate::render::SegmentStyle {
                color: Some(crate::render::ResolvedColor::Token(
                    crate::render::StyleToken::TextDim
                )),
                bold: None,
            })
        );
        assert!(resolved_normal.formats.get("cwd").is_none());
        assert_eq!(resolved_normal.cwd_normal, crate::render::CwdStyle::Short);
        assert_eq!(resolved_compact.cwd_compact, crate::render::CwdStyle::Name);
        assert_eq!(
            resolved_normal.styles.get("git"),
            Some(&crate::render::SegmentStyle {
                color: Some(crate::render::ResolvedColor::Token(
                    crate::render::StyleToken::TextDim
                )),
                bold: None,
            })
        );
        assert_eq!(resolved_normal.formats.get("git"), Some(&crate::render::SegmentFormat::Long));
        assert_eq!(resolved_compact.formats.get("git"), Some(&crate::render::SegmentFormat::Short));
        // Already present: never rewritten, user edits survive.
        fs::write(&path, "{\"layout\": \"compact\"}").unwrap();
        assert!(!ensure_default_config(&path));
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"layout\": \"compact\"}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bare_paths_stay_bare() {
        assert!(!needs_quoting("D:/Code/bin/kimi-code-hud-rs.exe"));
        assert!(!needs_quoting("/usr/local/bin/kimi-code-hud-rs"));
        assert!(needs_quoting("C:\\Program Files\\hud\\kimi-code-hud-rs.exe"));
    }

    #[test]
    fn spaced_paths_quote_and_escape() {
        assert_eq!(
            quote_and_escape("C:\\Program Files\\hud\\kimi-code-hud-rs.exe"),
            "\"C:\\\\Program Files\\\\hud\\\\kimi-code-hud-rs.exe\""
        );
    }

    #[test]
    #[cfg(windows)]
    fn short_path_strips_spaces_when_available() {
        // A real spaced directory: GetShortPathNameW should yield a space-free
        // 8.3 form; when the volume disables 8dot3 names the fallback is the
        // quoted form, which needs_quoting/exe_command_word handles.
        let dir = std::env::temp_dir().join(format!("kimi hud spaced {}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let word = exe_command_word(&dir.join("kimi-code-hud-rs.exe"));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !word.starts_with('"') || word.contains(' '),
            "either a bare short path or a quoted spaced path, got: {}",
            word
        );
    }

    #[test]
    fn own_command_survives_toml_roundtrip() {
        for exe in [
            "D:\\Code\\AiProjects\\kimi-code-hud-rs\\target\\release\\kimi-code-hud-rs.exe",
            "C:\\Program Files\\hud dir\\kimi-code-hud-rs.exe",
            "/opt/my tools/kimi-code-hud-rs",
        ] {
            let command = status_line_command(Path::new(exe));
            let toml = set_status_line_command("", &command);
            let decoded = get_status_line_command(&toml).expect(exe);
            assert_eq!(decoded, command, "TOML roundtrip must be lossless");
            assert!(is_own_command(&decoded), "removal must recognize its own command: {}", decoded);
        }
    }
}
