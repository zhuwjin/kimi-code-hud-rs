// Thinking-level resolution, mirroring the host's fallback chain:
// in-session wire change > per-session snapshot > [thinking] config >
// model default_effort > boolean "on". The snapshot pins a session's level
// so another session's /effort rewrite of the global config cannot leak in.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model_config::{
    bool_value, find_model_table, string_array_value, string_value, table_text,
};
use crate::util;

#[derive(Debug, Clone)]
pub struct ThinkingResolution {
    pub level: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Snapshot {
    level: String,
    #[serde(default)]
    model: Option<String>,
}

fn snapshot_path(snapshot_dir: &Path, session_id: &str) -> std::path::PathBuf {
    snapshot_dir.join(format!("thinking-{}.json", util::safe_component(session_id)))
}

fn read_snapshot(snapshot_dir: &Path, session_id: &str) -> Option<Snapshot> {
    let text = util::read_string(&snapshot_path(snapshot_dir, session_id))?;
    let snap = serde_json::from_str::<Snapshot>(&text).ok()?;
    (!snap.level.is_empty()).then_some(snap)
}

fn write_snapshot(snapshot_dir: &Path, session_id: &str, level: &str, model: &str) {
    let snap = Snapshot {
        level: level.to_string(),
        model: Some(model.to_string()),
    };
    if let Ok(text) = serde_json::to_string(&snap) {
        let _ = util::atomic_write(&snapshot_path(snapshot_dir, session_id), text.as_bytes());
    }
}

/// Resolve the level from config.toml: [thinking] config > model
/// default_effort > boolean "on", following the host's own resolution.
fn resolve_from_config(model: &str, config_text: &str) -> String {
    let thinking = table_text(config_text, "thinking");
    let model_table = find_model_table(config_text, model);
    let caps = model_table.as_deref().and_then(|t| string_array_value(t, "capabilities"));
    let always_thinking = caps
        .as_ref()
        .is_some_and(|caps| caps.iter().any(|c| c == "always_thinking"));
    let thinking_capable = always_thinking
        || caps
            .as_ref()
            .is_some_and(|caps| caps.iter().any(|c| c == "thinking"))
        || model_table
            .as_deref()
            .and_then(|t| bool_value(t, "adaptive_thinking"))
            == Some(true);

    // [thinking] enabled=false forces off — except on always_thinking models.
    if thinking.as_deref().and_then(|t| bool_value(t, "enabled")) == Some(false) && !always_thinking
    {
        return "off".to_string();
    }

    let global_effort = thinking.as_deref().and_then(|t| string_value(t, "effort"));
    let has_efforts = model_table
        .as_deref()
        .is_some_and(|t| crate::model_config::has_key(t, "support_efforts"));
    if !has_efforts {
        // Explicit capabilities without thinking resolve to 'off' upstream;
        // a configured global effort still shows on compatible protocols.
        if caps.is_some() && !thinking_capable {
            return if global_effort.is_some() { "on".to_string() } else { "off".to_string() };
        }
        return "on".to_string();
    }

    let model_default = model_table.as_deref().and_then(|t| string_value(t, "default_effort"));
    if always_thinking {
        // Skip 'off' values and fall back to the model's own default.
        return global_effort
            .filter(|effort| effort != "off")
            .or(model_default)
            .unwrap_or_else(|| "on".to_string());
    }
    global_effort
        .or(model_default)
        .unwrap_or_else(|| "on".to_string())
}

/// Resolve the thinking level to display.
pub fn resolve_thinking_level(
    session_level: Option<&str>,
    model: &str,
    session_id: Option<&str>,
    snapshot_dir: &Path,
    config_text: &str,
) -> ThinkingResolution {
    if let Some(level) = session_level.filter(|l| !l.is_empty()) {
        if let Some(session_id) = session_id {
            write_snapshot(snapshot_dir, session_id, level, model);
        }
        return ThinkingResolution { level: level.to_string() };
    }
    if let Some(session_id) = session_id {
        if let Some(snap) = read_snapshot(snapshot_dir, session_id) {
            if snap.model.as_deref() == Some(model) {
                return ThinkingResolution { level: snap.level };
            }
        }
    }
    let level = resolve_from_config(model, config_text);
    if let Some(session_id) = session_id {
        write_snapshot(snapshot_dir, session_id, &level, model);
    }
    ThinkingResolution { level }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kimi-hud-rs-think-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const CONFIG: &str = r#"
[thinking]
enabled = true
effort = "medium"

[models."kimi-code/k3"]
model = "k3"
display_name = "K3"
provider = "managed:kimi-code"
capabilities = [ "thinking" ]
support_efforts = [ "low", "high" ]
default_effort = "high"
"#;

    #[test]
    fn wire_level_wins_and_pins_snapshot() {
        let dir = temp_dir("wire");
        let res = resolve_thinking_level(Some("max"), "K3", Some("sess1"), &dir, CONFIG);
        assert_eq!(res.level, "max");
        // Later frames with no wire level read the pinned snapshot.
        let res = resolve_thinking_level(None, "K3", Some("sess1"), &dir, CONFIG);
        assert_eq!(res.level, "max");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_fallback_pins_snapshot() {
        let dir = temp_dir("config");
        let res = resolve_thinking_level(None, "K3", Some("sess2"), &dir, CONFIG);
        // global effort wins over the model default.
        assert_eq!(res.level, "medium");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn boolean_model_falls_back_to_on() {
        let res = resolve_thinking_level(None, "unknown-model", None, Path::new("."), "");
        assert_eq!(res.level, "on");
    }
}
