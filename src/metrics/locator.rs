// Locating the session directory under ~/.kimi-code/sessions across legacy
// and current host spellings (ses_<id>, session_<id>, bare id), one workspace
// directory deep.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn session_candidates(session_id: &str) -> Vec<String> {
    if session_id.is_empty() {
        return Vec::new();
    }
    let mut bare = session_id.to_string();
    for prefix in ["ses_", "session_"] {
        if let Some(stripped) = bare.strip_prefix(prefix) {
            bare = stripped.to_string();
            break;
        }
    }
    vec![format!("ses_{}", bare), format!("session_{}", bare), bare]
}

/// Scan the sessions root for the session directory; honors the deadline so a
// huge sessions tree cannot eat the render frame.
pub fn find_session_dir(session_id: &str, sessions_root: &Path, deadline: Instant) -> Option<PathBuf> {
    let candidates = session_candidates(session_id);
    if candidates.is_empty() {
        return None;
    }
    let entries = fs::read_dir(sessions_root).ok()?;
    for entry in entries {
        if Instant::now() >= deadline {
            return None;
        }
        let entry = entry.ok()?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        for name in &candidates {
            let candidate = entry.path().join(name);
            if fs::metadata(&candidate).map(|m| m.is_dir()).unwrap_or(false) {
                return Some(candidate);
            }
        }
    }
    None
}

fn cached_session_dir_valid(dir: Option<&str>, session_id: &str, sessions_root: &Path) -> Option<PathBuf> {
    let dir = dir?;
    let candidate = Path::new(dir);
    let root = sessions_root.canonicalize().unwrap_or_else(|_| sessions_root.to_path_buf());
    let resolved = candidate.canonicalize().ok()?;
    if !resolved.starts_with(&root) {
        return None;
    }
    if !session_candidates(session_id).contains(&resolved.file_name()?.to_string_lossy().to_string()) {
        return None;
    }
    if !resolved.is_dir() {
        return None;
    }
    Some(resolved)
}

/// Use the persisted directory when still valid, falling back to a scan.
pub fn resolve_session_dir(
    session_id: &str,
    sessions_root: &Path,
    cached: Option<&str>,
    deadline: Instant,
) -> Option<PathBuf> {
    cached_session_dir_valid(cached, session_id, sessions_root)
        .or_else(|| find_session_dir(session_id, sessions_root, deadline))
}
