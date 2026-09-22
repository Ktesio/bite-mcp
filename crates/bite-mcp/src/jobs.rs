//! Global job registry — Swift-side jobs (crawler, bulk ops) report progress
//! via bridge notifications; this registry mirrors the latest state so
//! `job_status` / `index_status` tools can answer without round-tripping to
//! the helper, and records batch-ingest accounting.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};

#[derive(Default)]
pub struct JobRegistry {
    inner: Mutex<HashMap<String, Value>>,
    pending_batches: Mutex<usize>,
}

static REGISTRY: std::sync::OnceLock<JobRegistry> = std::sync::OnceLock::new();

fn registry() -> &'static JobRegistry {
    REGISTRY.get_or_init(JobRegistry::default)
}

pub fn record(job_id: &str, state: &str, processed: i64, found: i64, kind: &str) {
    let mut map = registry().inner.lock().unwrap();
    map.insert(
        job_id.to_string(),
        json!({
            "job_id": job_id,
            "kind": kind,
            "state": state,
            "processed": processed,
            "found": found,
        }),
    );
}

pub fn set_pending_batches(n: usize) {
    *registry().pending_batches.lock().unwrap() = n;
}

pub fn snapshot() -> Value {
    let map = registry().inner.lock().unwrap();
    let mut jobs: Vec<&Value> = map.values().collect();
    jobs.sort_by_key(|j| j["job_id"].as_str().unwrap_or_default().to_string());
    json!({
        "jobs": jobs,
        "pending_batches": *registry().pending_batches.lock().unwrap(),
    })
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
