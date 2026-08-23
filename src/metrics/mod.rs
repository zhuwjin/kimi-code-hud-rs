// Metrics facade: locate the session's wire journals, advance every agent's
// bounded incremental cursor, fold the new rows, persist, and summarize.

pub mod locator;
pub mod reduce;
pub mod state;
pub mod summary;
pub mod wire;

#[allow(unused_imports)] // CacheMetric is part of the module API used by tests/callers
pub use summary::{CacheMetric, MetricsSummary};

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::Instant;

use serde_json::Value;
use state::{AgentBucket, CacheState, MetricsState};

/// Build one frame's summary for the session. Every IO or parse failure is
/// silent: the HUD renders with whatever information it managed to collect.
pub fn get_metrics(
    session_id: Option<&str>,
    sessions_root: &Path,
    state_dir: &Path,
    deadline: Instant,
    now: i64,
) -> MetricsSummary {
    let Some(session_id) = session_id.filter(|s| !s.is_empty()) else {
        let mut empty = MetricsState::new();
        return summary::summarize(&mut empty, now, &BTreeSet::new()).0;
    };
    let state_path = state::state_path_for(session_id, state_dir);
    let mut st = state::load(&state_path);
    st.v = state::METRICS_STATE_V;

    let Some(session_dir) =
        locator::resolve_session_dir(session_id, sessions_root, st.session_dir.as_deref(), deadline)
    else {
        st.session_dir = None;
        return summary::summarize(&mut st, now, &BTreeSet::new()).0;
    };
    st.session_dir = Some(session_dir.to_string_lossy().into_owned());

    // Discover every agent's wire.jsonl; main reads first so its slice is
    // never starved by subagent backfill.
    let agents_dir = session_dir.join("agents");
    let mut agent_wires: Vec<(String, std::path::PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&agents_dir) {
        for entry in entries.flatten() {
            let wire_path = entry.path().join("wire.jsonl");
            if wire_path.is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    agent_wires.push((name.to_string(), wire_path));
                }
            }
        }
    }
    agent_wires.sort_by(|a, b| {
        let a_main = a.0 == "main";
        let b_main = b.0 == "main";
        b_main.cmp(&a_main).then_with(|| a.0.cmp(&b.0))
    });

    let mut budget: u64 = wire::WIRE_READ_BUDGET_BYTES;
    let mut changed = false;
    let mut agent_names: BTreeSet<String> = BTreeSet::new();
    for (name, wire_path) in &agent_wires {
        let Ok(meta) = fs::metadata(wire_path) else { continue };
        let size = meta.len();
        agent_names.insert(name.clone());

        // Rotation or in-place truncation restarts this agent from byte 0;
        // the main agent's cache counters restart with it.
        {
            let bucket = st.agents.entry(name.clone()).or_default();
            let truncated = size < bucket.offset;
            let rotated = !truncated && !wire::wire_tail_matches(wire_path, bucket);
            if truncated || rotated {
                *bucket = AgentBucket::default();
                if name == "main" {
                    st.cache = CacheState::default();
                }
            }
        }

        let slice = if name == "main" {
            wire::MAIN_WIRE_SLICE_BYTES
        } else {
            wire::AGENT_WIRE_SLICE_BYTES
        };
        let max = slice.min(budget);
        let mut text: Option<String> = None;
        if max > 0 && Instant::now() < deadline {
            let before = st.agents.get(name).map(|b| b.offset).unwrap_or(0);
            if let Some(bucket) = st.agents.get_mut(name) {
                text = wire::read_bounded_wire(wire_path, bucket, size, max);
            }
            let after = st.agents.get(name).map(|b| b.offset).unwrap_or(before);
            let consumed = after.saturating_sub(before);
            budget = budget.saturating_sub(consumed);
            if consumed > 0 {
                changed = true;
            }
        }
        let Some(text) = text else { continue };
        for line in text.split('\n') {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(row) = serde_json::from_str::<Value>(line) {
                reduce::process_row(&mut st, &row, name);
            }
        }
    }

    let (result, summary_changed) = summary::summarize(&mut st, now, &agent_names);
    if changed || summary_changed {
        state::save(&state_path, &st);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kimi-hud-rs-metrics-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn incremental_reads_across_frames() {
        let root = temp_dir("incr");
        let session_dir = root.join("ws1").join("ses_abc");
        let agents = session_dir.join("agents").join("main");
        fs::create_dir_all(&agents).unwrap();
        let wire = agents.join("wire.jsonl");
        fs::write(
            &wire,
            concat!(
                "{\"type\":\"turn.prompt\",\"time\":1000}\n",
                "{\"type\":\"context.append_loop_event\",\"time\":1010,\"event\":{\"type\":\"step.end\",\"llmStreamDurationMs\":1000,\"usage\":{\"output\":22,\"inputOther\":100,\"inputCacheRead\":100,\"inputCacheCreation\":0}}}\n"
            ),
        )
        .unwrap();
        let state_dir = root.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let now = 1100;

        let s = get_metrics(Some("abc"), &root, &state_dir, Instant::now() + Duration::from_secs(5), now);
        assert_eq!(s.turn_started_at, Some(1000));
        assert!(s.cache.is_some());

        // Second frame: no new bytes, same summary, and the state file exists.
        let s2 = get_metrics(Some("abc"), &root, &state_dir, Instant::now() + Duration::from_secs(5), now + 50);
        assert_eq!(s2.turn_started_at, Some(1000));

        // Append task rows; the summary counts running background tasks.
        let append = concat!(
            "{\"type\":\"task.started\",\"info\":{\"taskId\":\"bash-1\",\"status\":\"running\"},\"time\":1500}\n",
            "{\"type\":\"task.started\",\"info\":{\"taskId\":\"agent-1\",\"status\":\"running\"},\"time\":1501}\n",
            "{\"type\":\"task.started\",\"info\":{\"taskId\":\"bash-2\",\"status\":\"running\"},\"time\":1502}\n",
            "{\"type\":\"task.terminated\",\"info\":{\"taskId\":\"bash-1\",\"status\":\"completed\"},\"time\":1600}\n",
            "{\"type\":\"turn.ended\",\"time\":2000}\n"
        );
        let mut f = fs::OpenOptions::new().append(true).open(&wire).unwrap();
        use std::io::Write;
        f.write_all(append.as_bytes()).unwrap();
        let s3 = get_metrics(Some("abc"), &root, &state_dir, Instant::now() + Duration::from_secs(5), 2100);
        assert_eq!(s3.turn_started_at, None, "turn ended closes the clock");
        assert_eq!(s3.bg_tasks, 1, "bash-2 still running, bash-1 retired");
        assert_eq!(s3.bg_agents, 1);
        // A day later with no terminated rows: the liveness horizon retires
        // both, the way a crash that never wrote task.terminated would.
        let s4 = get_metrics(
            Some("abc"),
            &root,
            &state_dir,
            Instant::now() + Duration::from_secs(5),
            2100 + 25 * 60 * 60 * 1000,
        );
        assert_eq!(s4.bg_tasks, 0);
        assert_eq!(s4.bg_agents, 0);

        let _ = fs::remove_dir_all(&root);
    }
}
