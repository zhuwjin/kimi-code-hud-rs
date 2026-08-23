// Turning the persisted reducer state into the stable summary the renderer
// consumes: fleet-aware TPS/TTFT, the user-turn clock, compaction state and
// the session cache hit rate.

use std::collections::BTreeSet;

use super::reduce::{
    ACTIVE_WINDOW_MS, MAX_SAMPLES, MIN_SAMPLES, SAMPLE_WINDOW_MS, TPS_TTL_MS,
};
use super::state::{AgentBucket, MetricsState, Sample};
use crate::util::median;

#[derive(Debug, Default, Clone)]
pub struct CacheMetric {
    pub hit_rate: f64,
}

#[derive(Debug, Default, Clone)]
pub struct MetricsSummary {
    pub tps: Option<f64>,
    pub tps_stale: bool,
    pub ttft_ms: Option<f64>,
    pub tps_total: Option<f64>,
    pub tps_agents: u32,
    pub active_agents: u32,
    pub main_active: bool,
    pub main_speed: bool,
    pub swarm_mode: bool,
    pub cache: Option<CacheMetric>,
    pub model_alias: Option<String>,
    pub thinking_level: Option<String>,
    pub turn_started_at: Option<i64>,
    pub compacting_since: Option<i64>,
    pub compaction_ms: Option<i64>,
    /// Background BPM tasks still running, split by kind like the host
    /// footer's badges.
    pub bg_tasks: u32,
    pub bg_agents: u32,
}

fn median_of_fresh(fresh: &[Sample]) -> Option<f64> {
    let values: Vec<f64> = super::reduce::last_n(fresh, MAX_SAMPLES).iter().map(|s| s.v).collect();
    median(&values)
}

/// Returns (summary, changed) — `changed` is true when stale samples were
/// dropped, signalling the caller to persist the state.
pub fn summarize(state: &mut MetricsState, now: i64, agent_names: &BTreeSet<String>) -> (MetricsSummary, bool) {
    // Pass 1: drop samples outside the 10-minute window.
    let names: Vec<String> = if agent_names.is_empty() {
        state.agents.keys().cloned().collect()
    } else {
        agent_names.iter().cloned().collect()
    };
    let mut changed = false;
    for name in &names {
        if let Some(bucket) = state.agents.get_mut(name) {
            let before = bucket.samples.len();
            bucket.samples.retain(|s| s.t >= now - SAMPLE_WINDOW_MS);
            if bucket.samples.len() != before {
                changed = true;
            }
        }
    }

    // Pass 2: classify agents and collect fleet figures.
    let mut active_speeds: Vec<f64> = Vec::new();
    let mut active_ttfts: Vec<f64> = Vec::new();
    let mut active_agents: u32 = 0;
    let mut main_active = false;
    let mut main_speed = false;
    let mut sole: Option<(&AgentBucket, Vec<Sample>)> = None;
    for name in &names {
        let Some(bucket) = state.agents.get(name) else { continue };
        let fresh: Vec<Sample> = bucket
            .samples
            .iter()
            .filter(|s| s.t >= now - SAMPLE_WINDOW_MS)
            .cloned()
            .collect();
        let speed = median_of_fresh(&fresh);
        let generating = bucket
            .last_request_at
            .is_some_and(|t| now - t < SAMPLE_WINDOW_MS && bucket.last_step_end_at.is_none_or(|e| t > e));
        let recent = fresh.last().is_some_and(|s| now - s.t <= ACTIVE_WINDOW_MS);
        // A subagent whose turn has ended leaves the fleet immediately; main
        // is exempt so its just-finished speed survives the recency window —
        // except in swarm mode, where a parked main (blocked inside the
        // swarm, no request in flight) must drop out too.
        let settled = name != "main"
            && bucket.last_turn_end_at.is_some_and(|te| {
                (fresh.is_empty() || te >= fresh.last().map(|s| s.t).unwrap_or(0))
                    && bucket.last_request_at.is_none_or(|r| te >= r)
            });
        let parked_main = name == "main" && state.swarm_mode && !generating;
        if parked_main || (!generating && (!recent || settled)) {
            continue;
        }
        active_agents += 1;
        sole = Some((bucket, fresh));
        if name == "main" {
            main_active = true;
        }
        if let Some(speed) = speed {
            active_speeds.push(speed);
            if name == "main" {
                main_speed = true;
            }
        }
        if let (Some(ttft), Some(last_sample_at)) = (bucket.last_ttft_ms, bucket.last_sample_at) {
            if last_sample_at >= now - SAMPLE_WINDOW_MS {
                active_ttfts.push(ttft);
            }
        }
    }

    let mut summary = MetricsSummary {
        swarm_mode: state.swarm_mode,
        model_alias: state.model_alias.clone(),
        thinking_level: state.thinking_level.clone(),
        active_agents,
        main_active,
        main_speed,
        ..MetricsSummary::default()
    };

    if active_agents >= 2 {
        if !active_speeds.is_empty() {
            let total: f64 = active_speeds.iter().sum();
            summary.tps_agents = active_speeds.len() as u32;
            summary.tps_total = Some(total);
            summary.tps = Some(total / summary.tps_agents as f64);
        }
        summary.ttft_ms = median(&active_ttfts);
    } else if active_agents == 1 {
        let Some((bucket, fresh)) = sole else {
            return (summary, changed);
        };
        let values: Vec<f64> = super::reduce::last_n(&fresh, MAX_SAMPLES).iter().map(|s| s.v).collect();
        let window_median = if fresh.len() >= MIN_SAMPLES { median(&values) } else { None };
        let fresh_enough = fresh.last().is_some_and(|s| now - s.t <= TPS_TTL_MS);
        if fresh_enough {
            match window_median {
                Some(median_value) => summary.tps = Some(median_value),
                None => {
                    // Provisional reading: fewer than MIN_SAMPLES fresh
                    // samples. Shown dimmed until the full median takes over.
                    summary.tps = median(&values);
                    summary.tps_stale = true;
                }
            }
        } else if let Some(last_median) = bucket.last_median {
            summary.tps = Some(last_median);
            summary.tps_stale = true;
        }
        if let Some(tps) = summary.tps {
            summary.tps_total = Some(tps);
            summary.tps_agents = 1;
        }
        summary.ttft_ms = bucket.last_ttft_ms;
    } else if let Some(main) = state.agents.get("main") {
        if let Some(last_median) = main.last_median {
            summary.tps = Some(last_median);
            summary.tps_stale = true;
        }
        summary.ttft_ms = main.last_ttft_ms;
    }

    let main = state.agents.get("main");
    summary.turn_started_at = main.and_then(|m| {
        m.last_turn_prompt_at.filter(|p| m.last_turn_end_at.is_none_or(|e| *p > e))
    });
    if let Some(m) = main {
        if let Some(begin) = m.last_compaction_begin_at {
            let open = m.last_compaction_end_at.is_none_or(|e| begin > e);
            if open && now - begin < SAMPLE_WINDOW_MS {
                summary.compacting_since = Some(begin);
            } else if let (Some(ms), Some(end)) = (m.last_compaction_ms, m.last_compaction_end_at) {
                if m.last_turn_prompt_at.is_none_or(|p| end > p) {
                    summary.compaction_ms = Some(ms);
                }
            }
        }
    }

    // A crash can leave a task.started without its terminated row; anything
    // older than the horizon is dead in practice (real tasks are seconds to
    // hours, never days).
    const TASK_LIVENESS_MS: i64 = 24 * 60 * 60 * 1000;
    let live = |id: &str| state.tasks.get(id).is_some_and(|started| now - started < TASK_LIVENESS_MS);
    summary.bg_tasks = state.tasks.keys().filter(|id| id.starts_with("bash-") && live(id)).count() as u32;
    summary.bg_agents = state.tasks.keys().filter(|id| id.starts_with("agent-") && live(id)).count() as u32;

    if state.cache.input_tokens > 0 {
        summary.cache = Some(CacheMetric {
            hit_rate: state.cache.read_tokens as f64 / state.cache.input_tokens as f64,
        });
    }
    (summary, changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::reduce::process_row;
    use serde_json::json;

    fn agent(name: &str) -> String {
        name.to_string()
    }

    #[test]
    fn solo_agent_tps_and_provisional() {
        let mut state = MetricsState::new();
        let t = 1_000_000i64;
        for i in 0..3 {
            let row = json!({
                "type": "context.append_loop_event",
                "time": t + i,
                "event": {
                    "type": "step.end",
                    "llmFirstTokenLatencyMs": 800.0,
                    "llmStreamDurationMs": 1000.0,
                    "usage": {"output": 40.0}
                }
            });
            process_row(&mut state, &row, "main");
        }
        let now = t + 500;
        let (summary, _) = summarize(&mut state, now, &BTreeSet::new());
        assert_eq!(summary.active_agents, 1, "recent samples keep main in the fleet");
        assert_eq!(summary.tps, Some(40.0));
        assert!(!summary.tps_stale, "3 samples make a full median");
        assert_eq!(summary.ttft_ms, Some(800.0));
    }

    #[test]
    fn fleet_totals_when_two_agents_active() {
        let mut state = MetricsState::new();
        let t = 1_000_000i64;
        for (name, speed) in [("main", 40.0), ("sub", 60.0)] {
            let request = json!({"type": "llm.request", "time": t});
            process_row(&mut state, &request, agent(name).as_str());
            for i in 0..3 {
                let row = json!({
                    "type": "context.append_loop_event",
                    "time": t + 1 + i,
                    "event": {
                        "type": "step.end",
                        "llmStreamDurationMs": 1000.0,
                        "usage": {"output": speed}
                    }
                });
                process_row(&mut state, &row, name);
            }
        }
        let now = t + 100;
        let (summary, _) = summarize(&mut state, now, &BTreeSet::new());
        assert_eq!(summary.active_agents, 2);
        assert_eq!(summary.tps_total, Some(100.0));
        assert_eq!(summary.tps, Some(50.0));
        assert!(summary.main_active);
        assert!(summary.main_speed);
    }

    #[test]
    fn turn_clock_and_cache_accumulate() {
        let mut state = MetricsState::new();
        let t = 1_000_000i64;
        process_row(&mut state, &json!({"type": "turn.prompt", "time": t}), "main");
        let step = json!({
            "type": "context.append_loop_event",
            "time": t + 10,
            "event": {
                "type": "step.end",
                "llmStreamDurationMs": 0.0,
                "usage": {"output": 5.0, "inputOther": 100.0, "inputCacheRead": 300.0, "inputCacheCreation": 100.0}
            }
        });
        process_row(&mut state, &step, "main");
        let (summary, _) = summarize(&mut state, t + 20, &BTreeSet::new());
        assert_eq!(summary.turn_started_at, Some(t));
        let cache = summary.cache.unwrap();
        assert!((cache.hit_rate - 0.6).abs() < 1e-9);
    }

    #[test]
    fn subagent_settles_after_end_turn() {
        let mut state = MetricsState::new();
        let t = 1_000_000i64;
        process_row(&mut state, &json!({"type": "llm.request", "time": t}), "sub");
        let step = json!({
            "type": "context.append_loop_event",
            "time": t + 5,
            "event": {
                "type": "step.end",
                "finishReason": "end_turn",
                "llmStreamDurationMs": 1000.0,
                "usage": {"output": 30.0}
            }
        });
        process_row(&mut state, &step, "sub");
        let (summary, _) = summarize(&mut state, t + 10, &BTreeSet::new());
        assert_eq!(summary.active_agents, 0, "settled subagent drops out immediately");
    }
}
