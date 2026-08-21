// The JSON snapshot the host TUI writes to stdin, one object per refresh.

use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Payload {
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub permission_mode: Option<String>,
    pub plan_mode: Option<bool>,
    pub session_id: Option<String>,
    pub version: Option<String>,
    pub swarm_mode: Option<bool>,
}

/// Parse a payload string; None on empty input, non-objects, or bad JSON.
pub fn parse_payload(text: &str) -> Option<Payload> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str::<Payload>(trimmed).ok()
}

/// Read stdin until EOF, the 1 MiB cap, or the timeout — whichever first —
/// then parse whatever arrived. A reader thread feeds a channel so the main
/// thread can stop waiting at the deadline without leaking the pipe.
pub fn read_stdin_payload(timeout: Duration, max_bytes: usize) -> Option<Payload> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut buf = [0u8; 8192];
        let mut total = 0usize;
        loop {
            match lock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                    if total > max_bytes {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let mut acc: Vec<u8> = Vec::new();
    let mut oversized = false;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(chunk) => {
                acc.extend_from_slice(&chunk);
                if acc.len() > max_bytes {
                    oversized = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if oversized {
        return None;
    }
    parse_payload(&String::from_utf8_lossy(&acc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_camel_case_fields() {
        let p = parse_payload(
            r#"{"model":"K3","cwd":"/tmp","gitBranch":"main","permissionMode":"yolo","planMode":false,"sessionId":"abc","version":"0.38.0"}"#,
        )
        .unwrap();
        assert_eq!(p.model.as_deref(), Some("K3"));
        assert_eq!(p.git_branch.as_deref(), Some("main"));
        assert_eq!(p.permission_mode.as_deref(), Some("yolo"));
        assert_eq!(p.session_id.as_deref(), Some("abc"));
    }

    #[test]
    fn rejects_empty_and_non_object() {
        assert!(parse_payload("").is_none());
        assert!(parse_payload("   ").is_none());
        assert!(parse_payload("[1,2]").is_none());
        assert!(parse_payload("not json").is_none());
    }

    #[test]
    fn ignores_unknown_fields() {
        let p = parse_payload(r#"{"model":"K3","futureField":123}"#).unwrap();
        assert_eq!(p.model.as_deref(), Some("K3"));
    }
}
