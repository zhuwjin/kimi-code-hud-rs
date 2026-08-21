// Small shared helpers: atomic writes, wall clock, hex, medians, and the
// terminal-control sanitizer every piece of untrusted display text passes
// through before the HUD applies its own ANSI styling.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// Current wall clock in Unix epoch milliseconds; 0 when the clock is wrong.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Lowercase hex of some bytes.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// SHA-256 hex digest of some bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Sanitize a session id (or any string) into a safe filename component.
pub fn safe_component(input: &str) -> String {
    input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Write a file atomically (temp file + rename). The temp name lives in the
/// target directory so the rename stays on one filesystem.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = dir.join(format!(".{}.tmp-{}", name, std::process::id()));
    fs::write(&tmp, bytes)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err)
        }
    }
}

/// Best-effort whole-file UTF-8 read; None when missing or unreadable.
pub fn read_string(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Median of a slice; empty input yields None. Even lengths average the two
/// middle values.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    })
}

fn skip_csi(chars: &[char], start: usize) -> usize {
    for (i, ch) in chars.iter().enumerate().skip(start) {
        let code = *ch as u32;
        if (0x40..=0x7e).contains(&code) {
            return i + 1;
        }
    }
    chars.len()
}

fn skip_string_control(chars: &[char], start: usize, bell_terminates: bool) -> usize {
    for (i, ch) in chars.iter().enumerate().skip(start) {
        let code = *ch as u32;
        if bell_terminates && code == 0x07 {
            return i + 1;
        }
        if code == 0x9c {
            return i + 1;
        }
        if code == 0x1b && chars.get(i + 1).map(|c| *c as u32) == Some(0x5c) {
            return i + 2;
        }
    }
    chars.len()
}

/// Remove terminal controls from untrusted display text: OSC/DCS/APC/PM/SOS
/// strings are dropped with their payload, CSI sequences and C0/DEL/C1
/// controls are dropped as controls. The HUD's own SGR output never passes
/// through here.
pub fn sanitize_terminal_text(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let code = chars[i] as u32;
        if code == 0x1b {
            let next = chars.get(i + 1).map(|c| *c as u32);
            match next {
                Some(0x5b) => i = skip_csi(&chars, i + 2),
                Some(0x5d) => i = skip_string_control(&chars, i + 2, true),
                Some(0x50 | 0x58 | 0x5e | 0x5f) => i = skip_string_control(&chars, i + 2, false),
                Some(n) if (0x20..=0x7e).contains(&n) => i += 2,
                _ => i += 1,
            }
            continue;
        }
        if code == 0x9b {
            i = skip_csi(&chars, i + 1);
            continue;
        }
        if code == 0x9d {
            i = skip_string_control(&chars, i + 1, true);
            continue;
        }
        if code == 0x90 || code == 0x98 || code == 0x9e || code == 0x9f {
            i = skip_string_control(&chars, i + 1, false);
            continue;
        }
        if code <= 0x1f || (0x7f..=0x9f).contains(&code) {
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Strip SGR sequences (`ESC [ params m`) — used to measure visible width.
pub fn strip_ansi_sgr(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\x1b' && chars.get(i + 1) == Some(&'[') {
            let mut j = i + 2;
            while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == ';') {
                j += 1;
            }
            if j < chars.len() && chars[j] == 'm' {
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizer_drops_osc_payload_and_csi() {
        assert_eq!(sanitize_terminal_text("ab\u{1b}]0;title\u{7}cd"), "abcd");
        assert_eq!(sanitize_terminal_text("a\u{1b}[31mb"), "ab");
        // ESC + one printable is treated as a two-byte escape command (as in
        // the Node sanitizer), so both bytes drop.
        assert_eq!(sanitize_terminal_text("x\u{1b}yz"), "xz");
        assert_eq!(sanitize_terminal_text("n\u{0}ul"), "nul");
    }

    #[test]
    fn sanitizer_keeps_plain_text() {
        assert_eq!(sanitize_terminal_text("K3 high"), "K3 high");
        assert_eq!(sanitize_terminal_text("中文 ✓"), "中文 ✓");
    }

    #[test]
    fn strip_sgr_removes_only_sgr() {
        assert_eq!(strip_ansi_sgr("\u{1b}[38;2;1;2;3mab\u{1b}[0m"), "ab");
        assert_eq!(strip_ansi_sgr("\u{1b}[2Jx"), "\u{1b}[2Jx");
    }

    #[test]
    fn median_odd_and_even() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn safe_component_replaces() {
        assert_eq!(safe_component("ses_abc/../../x"), "ses_abc_______x");
    }
}
