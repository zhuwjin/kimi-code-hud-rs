// Folding parsed wire.jsonl rows into the metrics state. Rows are bucketed
// per agent; the main agent additionally feeds the session cache counters,
// model/thinking metadata and the swarm flag.

use serde_json::Value;

use super::state::{AgentBucket, MetricsState, Sample};
use crate::util::median;

pub const MAX_SAMPLES: usize = 5;
pub const MIN_SAMPLES: usize = 3;
pub const MIN_STREAM_MS: f64 = 250.0;
pub const MAX_TPS: f64 = 1000.0;
pub const TPS_TTL_MS: i64 = 2 * 60 * 1000;
pub const SAMPLE_WINDOW_MS: i64 = 10 * 60 * 1000;
pub const ACTIVE_WINDOW_MS: i64 = TPS_TTL_MS;
pub const MAX_STORED_SAMPLES: usize = 20;

fn row_time(row: &Value) -> Option<i64> {
    row.get("time")
        .and_then(|v| v.as_f64())
        .filter(|t| t.is_finite() && *t >= 0.0)
        .map(|t| t as i64)
}

fn row_type(row: &Value) -> Option<&str> {
    row.get("type").and_then(|v| v.as_str())
}

fn str_field<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Fold one parsed wire row. Row handlers run in the same order as the Node
/// implementation so interdependent timestamps settle identically.
pub fn process_row(state: &mut MetricsState, row: &Value, agent: &str) {
    if agent == "main" {
        apply_cache_row(state, row);
    }
    apply_turn_row(state, row, agent);
    apply_throughput_row(state, row, agent);
    apply_compaction_row(state, row, agent);
    apply_session_meta_row(state, row, agent);
    apply_task_row(state, row);
}

/// Background BPM task lifecycle (bash-* shells, agent-* subagents) from any
/// agent's wire: started records id → startedAt, terminated retires it.
fn apply_task_row(state: &mut MetricsState, row: &Value) {
    let kind = row_type(row);
    if !matches!(kind, Some("task.started") | Some("task.terminated")) {
        return;
    }
    let Some(info) = row.get("info") else { return };
    let Some(id) = str_field(info, "taskId") else { return };
    match kind {
        Some("task.started") => {
            let started_at = info
                .get("startedAt")
                .and_then(|v| v.as_f64())
                .map(|t| t as i64)
                .or_else(|| row_time(row))
                .unwrap_or_default();
            state.tasks.insert(id.to_string(), started_at);
        }
        Some("task.terminated") => {
            state.tasks.remove(id);
        }
        _ => {}
    }
}

/// Session-cumulative cache counters from the main agent's step.end usage.
/// A step with incomplete usage fields is skipped rather than poisoning the
/// ratio.
fn apply_cache_row(state: &mut MetricsState, row: &Value) {
    if row_type(row) != Some("context.append_loop_event") {
        return;
    }
    let Some(event) = row.get("event") else { return };
    if event.get("type").and_then(|v| v.as_str()) != Some("step.end") {
        return;
    }
    let Some(usage) = event.get("usage") else { return };
    let fields = ["inputOther", "inputCacheRead", "inputCacheCreation"].map(|key| {
        usage
            .get(key)
            .and_then(|v| v.as_f64())
            .filter(|n| n.is_finite() && *n >= 0.0)
    });
    let [Some(other), Some(read), Some(creation)] = fields else {
        return;
    };
    state.cache.read_tokens += read as u64;
    state.cache.input_tokens += (other + read + creation) as u64;
}

/// The user-turn clock, main-only for the prompt anchor. A subagent's closing
/// end_turn step.end lets the fleet summary drop it the moment it finishes
/// instead of waiting out the recency window.
fn apply_turn_row(state: &mut MetricsState, row: &Value, agent: &str) {
    let Some(t) = row_time(row) else { return };
    let kind = row_type(row);
    let bucket = state.agents.entry(agent.to_string()).or_default();
    if agent == "main" && kind == Some("turn.prompt") {
        if bucket.last_turn_prompt_at.is_none_or(|p| t > p) {
            bucket.last_turn_prompt_at = Some(t);
        }
        return;
    }
    let active_cancel = kind == Some("turn.cancel") && row.get("target").and_then(|v| v.as_str()) != Some("queued");
    if kind == Some("turn.ended") || active_cancel {
        if bucket.last_turn_end_at.is_none_or(|e| t > e) {
            bucket.last_turn_end_at = Some(t);
        }
        return;
    }
    if kind == Some("context.append_loop_event") {
        let event = row.get("event");
        let is_end_turn = event
            .map(|e| {
                e.get("type").and_then(|v| v.as_str()) == Some("step.end")
                    && e.get("finishReason").and_then(|v| v.as_str()) == Some("end_turn")
            })
            .unwrap_or(false);
        if is_end_turn && bucket.last_turn_end_at.is_none_or(|e| t > e) {
            bucket.last_turn_end_at = Some(t);
        }
    }
}

/// Request lifecycle, TTFT and TPS samples for one agent.
fn apply_throughput_row(state: &mut MetricsState, row: &Value, agent: &str) {
    let t = row_time(row);
    let kind = row_type(row);

    if kind == Some("llm.request") {
        if row.get("kind").and_then(|v| v.as_str()) != Some("compaction") {
            if let Some(t) = t {
                let bucket = state.agents.entry(agent.to_string()).or_default();
                if bucket.last_request_at.is_none_or(|r| t > r) {
                    bucket.last_request_at = Some(t);
                }
            }
        }
        return;
    }

    let active_cancel = kind == Some("turn.cancel") && row.get("target").and_then(|v| v.as_str()) != Some("queued");
    if kind == Some("turn.ended") || kind == Some("full_compaction.complete") || active_cancel {
        if let Some(t) = t {
            let bucket = state.agents.entry(agent.to_string()).or_default();
            if bucket.last_step_end_at.is_none_or(|s| t > s) {
                bucket.last_step_end_at = Some(t);
            }
        }
        return;
    }

    if kind != Some("context.append_loop_event") {
        return;
    }
    let Some(event) = row.get("event") else { return };
    if event.get("type").and_then(|v| v.as_str()) != Some("step.end") {
        return;
    }
    let bucket = state.agents.entry(agent.to_string()).or_default();
    if let Some(t) = t {
        if bucket.last_step_end_at.is_none_or(|s| t > s) {
            bucket.last_step_end_at = Some(t);
        }
    }
    if let Some(ttft) = event
        .get("llmFirstTokenLatencyMs")
        .and_then(|v| v.as_f64())
        .filter(|n| n.is_finite() && *n >= 0.0)
    {
        bucket.last_ttft_ms = Some(ttft);
    }
    let output = event
        .pointer("/usage/output")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let stream_ms = event
        .get("llmStreamDurationMs")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let tps = if stream_ms > 0.0 {
        output / (stream_ms / 1000.0)
    } else {
        f64::INFINITY
    };
    if !output.is_finite()
        || output <= 0.0
        || !stream_ms.is_finite()
        || stream_ms < MIN_STREAM_MS
        || !tps.is_finite()
        || tps > MAX_TPS
    {
        return;
    }
    let Some(t) = t else { return };
    if bucket.last_sample_at.is_some_and(|last| t - last > TPS_TTL_MS) {
        bucket.samples.clear();
    }
    bucket.samples.push(Sample { v: tps, t });
    if bucket.samples.len() > MAX_STORED_SAMPLES {
        let drop = bucket.samples.len() - MAX_STORED_SAMPLES;
        bucket.samples.drain(0..drop);
    }
    bucket.last_sample_at = Some(t);
    if bucket.samples.len() >= MIN_SAMPLES {
        let values: Vec<f64> = last_n(&bucket.samples, MAX_SAMPLES).iter().map(|s| s.v).collect();
        bucket.last_median = median(&values);
    }
}

/// Manual/between-turn full-compaction timing, main agent only.
fn apply_compaction_row(state: &mut MetricsState, row: &Value, agent: &str) {
    if agent != "main" {
        return;
    }
    let Some(t) = row_time(row) else { return };
    let kind = row_type(row);
    let bucket: &mut AgentBucket = state.agents.entry("main".to_string()).or_default();

    if kind == Some("full_compaction.begin") {
        let turn_in_flight = bucket.last_turn_prompt_at.is_some_and(|p| {
            bucket.last_turn_end_at.is_none() || p > bucket.last_turn_end_at.unwrap()
        });
        if !turn_in_flight && bucket.last_compaction_begin_at.is_none_or(|b| t > b) {
            bucket.last_compaction_begin_at = Some(t);
        }
        return;
    }
    let open = bucket.last_compaction_begin_at.is_some_and(|b| {
        bucket.last_compaction_end_at.is_none() || b > bucket.last_compaction_end_at.unwrap()
    });
    if kind == Some("full_compaction.cancel") {
        if open {
            bucket.last_compaction_end_at = Some(t);
        }
        return;
    }
    if kind == Some("full_compaction.complete")
        && open
        && bucket.last_compaction_begin_at.is_some_and(|b| t >= b)
    {
        bucket.last_compaction_end_at = Some(t);
        bucket.last_compaction_ms = Some(t - bucket.last_compaction_begin_at.unwrap());
    }
}

fn reset_fleet_windows(state: &mut MetricsState) {
    for bucket in state.agents.values_mut() {
        bucket.samples.clear();
        bucket.last_ttft_ms = None;
        bucket.last_sample_at = None;
        bucket.last_median = None;
    }
}

/// Speed readings are not comparable across models: adopting a new alias
/// discards the fleet speed windows.
fn adopt_model_alias(state: &mut MetricsState, alias: Option<&str>) {
    let Some(alias) = alias.filter(|a| !a.is_empty()) else { return };
    if state.model_alias.as_deref() == Some(alias) {
        return;
    }
    let has_samples = state
        .agents
        .values()
        .any(|b| !b.samples.is_empty() || b.last_ttft_ms.is_some());
    if state.model_alias.is_some() || has_samples {
        reset_fleet_windows(state);
    }
    state.model_alias = Some(alias.to_string());
}

/// Main-wire model/thinking/swarm metadata. Per-request llm.request rows are
/// the ground truth: an in-session switch that emits no config row still
/// surfaces on the next request.
fn apply_session_meta_row(state: &mut MetricsState, row: &Value, agent: &str) {
    if agent != "main" {
        return;
    }
    let kind = row_type(row);
    if matches!(kind, Some("config.update") | Some("profile.bind")) {
        adopt_model_alias(state, str_field(row, "modelAlias"));
        let level = str_field(row, "thinkingEffort").or_else(|| str_field(row, "thinkingLevel"));
        if let Some(level) = level {
            state.thinking_level = Some(level.to_string());
        }
        return;
    }
    if kind == Some("llm.request") {
        adopt_model_alias(state, str_field(row, "modelAlias"));
        if let Some(level) = str_field(row, "thinkingEffort") {
            state.thinking_level = Some(level.to_string());
        }
        return;
    }
    if matches!(kind, Some("swarm_mode.enter") | Some("swarm_mode.exit")) {
        state.swarm_mode = kind == Some("swarm_mode.enter");
    }
}

pub(crate) fn last_n<T>(items: &[T], n: usize) -> &[T] {
    let start = items.len().saturating_sub(n);
    &items[start..]
}
