//! Global job registry — Swift-side jobs (crawler, bulk ops) report progress
//! via bridge notifications; this registry mirrors the latest state so the
//! `index_status` tool can answer without round-tripping to the helper, and
//! records batch-ingest accounting.
//!
//! Retention: running jobs are always kept; terminal jobs (`done`, `failed`,
//! `cancelled`) are capped at [`TERMINAL_RETENTION`] entries, oldest first,
//! so a long-lived server does not accumulate every job it has ever seen in
//! each `index_status` response.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};

/// Terminal states reported by the Swift helpers. A job in any other state
/// (e.g. `running`) may report again and is never pruned.
const TERMINAL_STATES: [&str; 3] = ["done", "failed", "cancelled"];

/// How many terminal jobs the registry keeps.
const TERMINAL_RETENTION: usize = 100;

struct JobEntry {
    /// Monotonic record order, bumped on every `record` call; the oldest
    /// terminal entries are pruned first.
    seq: u64,
    job: Value,
}

struct RegistryInner {
    jobs: HashMap<String, JobEntry>,
    next_seq: u64,
    terminal_retention: usize,
}

impl Default for RegistryInner {
    fn default() -> Self {
        Self {
            terminal_retention: TERMINAL_RETENTION,
            ..Self::empty()
        }
    }
}

impl RegistryInner {
    fn empty() -> Self {
        Self {
            jobs: HashMap::new(),
            next_seq: 0,
            terminal_retention: 0,
        }
    }

    fn record(&mut self, job_id: &str, state: &str, processed: i64, found: i64, kind: &str) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.jobs.insert(
            job_id.to_string(),
            JobEntry {
                seq,
                job: json!({
                    "job_id": job_id,
                    "kind": kind,
                    "state": state,
                    "processed": processed,
                    "found": found,
                }),
            },
        );
        if is_terminal(state) {
            self.prune_terminal();
        }
    }

    /// Drop the oldest terminal jobs beyond the retention cap. Running jobs
    /// are never dropped — their final state has not arrived yet.
    fn prune_terminal(&mut self) {
        let mut terminal: Vec<(u64, String)> = self
            .jobs
            .iter()
            .filter(|(_, entry)| entry.job["state"].as_str().is_some_and(is_terminal))
            .map(|(id, entry)| (entry.seq, id.clone()))
            .collect();
        if terminal.len() <= self.terminal_retention {
            return;
        }
        let excess = terminal.len() - self.terminal_retention;
        terminal.sort_unstable_by_key(|(seq, _)| *seq);
        terminal.truncate(excess);
        for (_, id) in terminal {
            self.jobs.remove(&id);
        }
    }
}

fn is_terminal(state: &str) -> bool {
    TERMINAL_STATES.contains(&state)
}

#[derive(Default)]
pub struct JobRegistry {
    inner: Mutex<RegistryInner>,
    pending_batches: Mutex<usize>,
}

static REGISTRY: std::sync::OnceLock<JobRegistry> = std::sync::OnceLock::new();

fn registry() -> &'static JobRegistry {
    REGISTRY.get_or_init(JobRegistry::default)
}

impl JobRegistry {
    fn record(&self, job_id: &str, state: &str, processed: i64, found: i64, kind: &str) {
        self.inner
            .lock()
            .unwrap()
            .record(job_id, state, processed, found, kind);
    }

    fn snapshot(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        let mut jobs: Vec<&Value> = inner.jobs.values().map(|entry| &entry.job).collect();
        jobs.sort_by_key(|j| j["job_id"].as_str().unwrap_or_default().to_string());
        json!({
            "jobs": jobs,
            "pending_batches": *self.pending_batches.lock().unwrap(),
        })
    }
}

pub fn record(job_id: &str, state: &str, processed: i64, found: i64, kind: &str) {
    registry().record(job_id, state, processed, found, kind);
}

pub fn set_pending_batches(n: usize) {
    *registry().pending_batches.lock().unwrap() = n;
}

pub fn snapshot() -> Value {
    registry().snapshot()
}

/// Bridge notification entry point. Called from the bridge reader thread —
/// must be fast; heavy work (batch ingestion) is caller's responsibility.
pub fn handle_notification(method: &str, params: &Value) {
    if method == "job_progress" {
        let job_id = params
            .get("job_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let state = params
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("running");
        let processed = params
            .get("processed")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let found = params.get("found").and_then(|v| v.as_i64()).unwrap_or(0);
        let kind = params.get("kind").and_then(|v| v.as_str()).unwrap_or("job");
        record(job_id, state, processed, found, kind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with_retention(retention: usize) -> JobRegistry {
        JobRegistry {
            inner: Mutex::new(RegistryInner {
                terminal_retention: retention,
                ..RegistryInner::empty()
            }),
            pending_batches: Mutex::new(0),
        }
    }

    fn job_ids(reg: &JobRegistry) -> Vec<String> {
        reg.snapshot()["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["job_id"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn terminal_jobs_beyond_retention_are_pruned_oldest_first() {
        let reg = registry_with_retention(3);
        for i in 0..5 {
            reg.record(&format!("t{i}"), "done", i, i, "mail-crawl");
        }
        assert_eq!(job_ids(&reg), ["t2", "t3", "t4"]);
    }

    #[test]
    fn running_jobs_are_never_pruned() {
        let reg = registry_with_retention(2);
        reg.record("live", "running", 1, 1, "mail-crawl");
        for i in 0..4 {
            reg.record(&format!("t{i}"), "failed", i, i, "mail-crawl");
        }
        assert_eq!(job_ids(&reg), ["live", "t2", "t3"]);
    }

    #[test]
    fn re_recording_updates_state_without_duplicating() {
        let reg = registry_with_retention(10);
        reg.record("j", "running", 1, 1, "mail-crawl");
        reg.record("j", "done", 5, 5, "mail-crawl");
        let snap = reg.snapshot();
        let jobs = snap["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["state"], "done");
        assert_eq!(jobs[0]["processed"], 5);
        assert_eq!(jobs[0]["found"], 5);
    }

    #[test]
    fn re_recording_a_terminal_job_refreshes_its_retention_slot() {
        let reg = registry_with_retention(2);
        reg.record("early", "done", 0, 0, "mail-crawl");
        reg.record("mid", "done", 0, 0, "mail-crawl");
        reg.record("late", "done", 0, 0, "mail-crawl"); // evicts "early"
        assert_eq!(job_ids(&reg), ["late", "mid"]);
        reg.record("mid", "done", 9, 9, "mail-crawl"); // refresh bumps its seq
        reg.record("new", "done", 0, 0, "mail-crawl"); // evicts "late", not "mid"
        assert_eq!(job_ids(&reg), ["mid", "new"]);
    }

    #[test]
    fn snapshot_keeps_job_id_sort_and_client_facing_shape() {
        let reg = registry_with_retention(10);
        for id in ["c", "a", "b"] {
            reg.record(id, "running", 0, 0, "kind");
        }
        let snap = reg.snapshot();
        assert_eq!(job_ids(&reg), ["a", "b", "c"]);
        assert_eq!(snap["pending_batches"], 0);
        let first = &snap["jobs"][0];
        assert_eq!(first["job_id"], "a");
        assert_eq!(first["kind"], "kind");
        assert_eq!(first["state"], "running");
        assert_eq!(first["processed"], 0);
        assert_eq!(first["found"], 0);
        // Exactly the five client-facing keys — the internal seq must not leak.
        let mut keys: Vec<&str> = first
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["found", "job_id", "kind", "processed", "state"]);
    }

    #[test]
    fn cancelled_is_terminal() {
        let reg = registry_with_retention(1);
        reg.record("first", "cancelled", 0, 0, "mail-crawl");
        reg.record("second", "cancelled", 0, 0, "mail-crawl");
        assert_eq!(job_ids(&reg), ["second"]);
    }
}
