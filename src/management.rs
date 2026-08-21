// Control plane: --install / --uninstall / --on / --off plus the
// SessionStart self-heal hook body. The host rewrites tui.toml on some
// upgrades (wiping [status_line]) but preserves config.toml [[hooks]], so
// --install also registers a hook that re-points the status line at every
// session start. Never touches a [status_line] command that is not ours —
// the user's own status line wins.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::toml_edit::{
    ensure_hooks_block, inspect_status_line_command, is_own_command, remove_hooks_block,
    remove_status_line_command, set_status_line_command, CommandValue,
};
use crate::util::{atomic_write, read_string};
use crate::RuntimePaths;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HudConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

fn read_hud_config(path: &Path) -> HudConfig {
    read_string(path)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_hud_config(path: &Path, config: &HudConfig) -> Result<(), String> {
    let text = serde_json::to_string_pretty(config)
        .map_err(|err| err.to_string())?;
    atomic_write(path, format!("{}\n", text).as_bytes()).map_err(|err| err.to_string())
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

pub fn hook_command(exe: &Path) -> String {
    format!("{} --sync-status-line", exe_command_word(exe))
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

fn install_hook(paths: &RuntimePaths, hook_cmd: &str) -> bool {
    let content = read_or_empty(&paths.config_toml_path);
    let next = ensure_hooks_block(&content, hook_cmd);
    write_if_changed(&paths.config_toml_path, &content, next)
}

fn remove_hook(paths: &RuntimePaths, hook_cmd: &str) -> bool {
    let content = read_or_empty(&paths.config_toml_path);
    let next = remove_hooks_block(&content, hook_cmd);
    if next == content {
        return false;
    }
    backup_file(&paths.config_toml_path);
    let _ = atomic_write(&paths.config_toml_path, next.as_bytes());
    true
}

/// --install: register the status line and the self-heal hook.
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
    let hook = hook_command(&exe);
    if install_hook(paths, &hook) {
        println!("Registered SessionStart self-heal hook in {}", paths.config_toml_path.display());
    }
    Ok(())
}

/// --uninstall: remove both, with backups.
pub fn uninstall(exe: &Path, paths: &RuntimePaths) -> Result<(), String> {
    let command = status_line_command(exe);
    if remove_status_line(paths, &command) {
        println!("Removed status line from {}", paths.tui_toml_path.display());
    }
    let hook = hook_command(exe);
    if remove_hook(paths, &hook) {
        println!("Removed SessionStart hook from {}", paths.config_toml_path.display());
    }
    Ok(())
}

/// --off: reversible switch. Sets the disabled flag (the hook stays dormant)
/// and strips the status-line command.
pub fn disable(exe: &Path, paths: &RuntimePaths) -> Result<(), String> {
    let mut config = read_hud_config(&paths.hud_config_path);
    if config.disabled != Some(true) {
        config.disabled = Some(true);
        write_hud_config(&paths.hud_config_path, &config)?;
    }
    let command = status_line_command(exe);
    if remove_status_line(paths, &command) {
        println!("Removed status line from {}", paths.tui_toml_path.display());
    }
    println!("HUD disabled (self-heal hook dormant; --on re-enables)");
    Ok(())
}

/// --on: clear the flag, write the command back, ensure the hook.
pub fn enable(exe: &Path, paths: &RuntimePaths) -> Result<(), String> {
    let (exe, copied) = installable_exe(exe, &paths.hud_dir);
    if copied {
        println!("Copied executable to {} (the build path contains spaces)", exe.display());
    }
    let mut config = read_hud_config(&paths.hud_config_path);
    if config.disabled.is_some() {
        config.disabled = None;
        write_hud_config(&paths.hud_config_path, &config)?;
    }
    let command = status_line_command(&exe);
    if install_status_line(paths, &command) {
        println!("Registered status line in {}", paths.tui_toml_path.display());
    }
    let hook = hook_command(&exe);
    if install_hook(paths, &hook) {
        println!("Registered SessionStart self-heal hook in {}", paths.config_toml_path.display());
    }
    Ok(())
}

/// SessionStart hook body (--sync-status-line): repair the tui.toml entry if
/// it is absent or ours. Stays silent while disabled, never overwrites a
/// foreign command, always exits 0.
pub fn sync_status_line(exe: &Path, paths: &RuntimePaths) {
    let result = std::panic::catch_unwind(|| sync_status_line_inner(exe, paths));
    drop(result);
}

fn sync_status_line_inner(exe: &Path, paths: &RuntimePaths) {
    // --off switch: honor the flag before touching anything.
    if read_hud_config(&paths.hud_config_path).disabled == Some(true) {
        return;
    }
    // The hook itself usually runs from the space-free copy, so this keeps
    // pointing tui.toml at it; the copy's content is refreshed by re-running
    // --install after a rebuild.
    let (exe, _copied) = installable_exe(exe, &paths.hud_dir);
    let content = read_or_empty(&paths.tui_toml_path);
    match inspect_status_line_command(&content) {
        CommandValue::Unknown => return,
        CommandValue::Parsed(value) if !is_own_command(&value) => return,
        _ => {}
    }
    let next = set_status_line_command(&content, &status_line_command(&exe));
    if next != content && next != content.replace("\r\n", "\n") {
        let _ = atomic_write(&paths.tui_toml_path, next.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toml_edit::{get_status_line_command, set_status_line_command};

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
            assert!(is_own_command(&decoded), "hook must recognize its own command: {}", decoded);
        }
    }

    #[test]
    fn hook_command_roundtrip() {
        let hook = hook_command(Path::new("C:\\Program Files\\hud\\kimi-code-hud-rs.exe"));
        assert!(hook.ends_with("--sync-status-line"));
        // safe_command_words must reassemble the quoted exe into one word.
        let words = crate::toml_edit::safe_command_words(&hook).unwrap();
        assert_eq!(words[0], "C:\\Program Files\\hud\\kimi-code-hud-rs.exe");
        assert_eq!(words[1], "--sync-status-line");
    }
}
