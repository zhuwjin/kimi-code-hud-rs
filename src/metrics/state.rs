// Persisted per-session metrics state: one wire-reader cursor per agent plus
// the session-cumulative cache counters. Only byte offsets, token counts and
// speed samples live here — never prompt or response text.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::util;

pub const METRICS_STATE_V: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub v: f64,
    pub t: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentBucket {
    #[serde(default)]
    pub offset: u64,
    /// SHA-256 hex of the bytes just before `offset` — detects in-place
    /// truncate-and-regrow that size checks alone cannot see.
    #[serde(default)]
    pub tail: Option<String>,
    /// Trailing partial line bytes, persisted so a JSONL row split across two
    /// status-line processes reassembles losslessly.
    #[serde(default)]
    pub pending: Vec<u8>,
    #[serde(default)]
    pub discarding: bool,
    #[serde(default)]
    pub samples: Vec<Sample>,
    #[serde(default)]
    pub last_median: Option<f64>,
    #[serde(default)]
    pub last_ttft_ms: Option<f64>,
    #[serde(default)]
    pub last_sample_at: Option<i64>,
    #[serde(default)]
    pub last_request_at: Option<i64>,
    #[serde(default)]
    pub last_step_end_at: Option<i64>,
    #[serde(default)]
    pub last_turn_prompt_at: Option<i64>,
    #[serde(default)]
    pub last_turn_end_at: Option<i64>,
    #[serde(default)]
    pub last_compaction_begin_at: Option<i64>,
    #[serde(default)]
    pub last_compaction_end_at: Option<i64>,
    #[serde(default)]
    pub last_compaction_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheState {
    #[serde(default)]
    pub read_tokens: u64,
    #[serde(default)]
    pub input_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsState {
    pub v: u32,
    #[serde(default)]
    pub agents: HashMap<String, AgentBucket>,
    #[serde(default)]
    pub session_dir: Option<String>,
    #[serde(default)]
    pub model_alias: Option<String>,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub swarm_mode: bool,
    #[serde(default)]
    pub cache: CacheState,
}

pub fn state_path_for(session_id: &str, state_dir: &Path) -> std::path::PathBuf {
    state_dir.join(format!("metrics-{}.json", util::safe_component(session_id)))
}

/// Load the state file; a missing, corrupt, or future-version file starts
/// clean (the bounded incremental reader then rebuilds from byte 0).
pub fn load(path: &Path) -> MetricsState {
    let Some(text) = util::read_string(path) else {
        return MetricsState::default();
    };
    serde_json::from_str::<MetricsState>(&text)
        .ok()
        .filter(|state| state.v == METRICS_STATE_V)
        .unwrap_or_default()
}

/// Best-effort atomic save; silent on failure like every hot-path write.
pub fn save(path: &Path, state: &MetricsState) {
    if let Ok(text) = serde_json::to_string(state) {
        let _ = util::atomic_write(path, text.as_bytes());
    }
}

impl MetricsState {
    pub fn new() -> Self {
        MetricsState {
            v: METRICS_STATE_V,
            ..MetricsState::default()
        }
    }
}
