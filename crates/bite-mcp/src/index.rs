//! Entity-index integration for the control plane: the LanceDB store, the
//! staged-batch ingester, the local (non-helper) index tools, and the
//! index-backed fast path for Mail search.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use bite_bridge::BridgeError;
use bite_core::error::cli_error;
use bite_core::BiteError;
use serde_json::{json, Value};

use crate::helper::BridgeHandle;

static INDEX: Mutex<Option<bite_index::EntityIndex>> = Mutex::new(None);
/// Epoch-ms of the last crawl spawn attempt (auto-refresh or explicit).
static LAST_SPAWN_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Epoch-ms of the last opportunistic auto-ingest attempt.
static LAST_AUTO_INGEST_MS: AtomicU64 = AtomicU64::new(0);
/// Set while `ingest_pending` runs its own (reporting) ingest, so the
/// opportunistic pass inside `with_index` stands down and lets it report.
static IN_INGEST_PENDING: AtomicBool = AtomicBool::new(false);

/// Minimum gap between opportunistic auto-ingest attempts per process.
const AUTO_INGEST_THROTTLE_MS: u64 = 30_000;

/// TTL for the cached crawler-liveness verdict on READ-ONLY paths. Two mail
/// tool calls within this window pay at most one kill+ps probe.
const CRAWLER_LIVENESS_TTL_MS: u64 = 5_000;

/// A cached alive verdict: the pidfile TEXT it was computed from (a
/// rewritten pidfile means a new worker — the cache must miss), the pid it
/// vouches for, and when it was taken.
struct LivenessHit {
    key: String,
    pid: i32,
    at_ms: u64,
}

static LIVENESS_CACHE: Mutex<Option<LivenessHit>> = Mutex::new(None);

/// Last background index failures (auto-ingest, post-crawl FTS refresh).
/// Those paths deliberately never fail the tool call that carried them —
/// without this record the failure is visible only on this process's
/// stderr. `index_status` surfaces the tail as `last_errors`.
static LAST_INDEX_ERRORS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Cap so a repeating failure can't grow the status payload unboundedly.
const LAST_ERRORS_MAX: usize = 8;

/// RAII guard for `IN_INGEST_PENDING`: set on acquire, cleared on drop —
/// panic-safe, so a panicking ingest can never wedge this process's
/// opportunistic ingest path permanently.
struct IngestPendingGuard;

impl IngestPendingGuard {
    fn acquire() -> Self {
        IN_INGEST_PENDING.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for IngestPendingGuard {
    fn drop(&mut self) {
        IN_INGEST_PENDING.store(false, Ordering::SeqCst);
    }
}

fn dir() -> std::path::PathBuf {
    bite_core::config::data_dir().join("index.lance")
}

fn staging() -> std::path::PathBuf {
    bite_core::config::data_dir().join("batches")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Gate for the opportunistic auto-ingest: staged batches exist AND enough
/// time has passed since this process's last attempt (hot tool loops must
/// not dir-scan constantly).
fn auto_ingest_due(last_attempt_ms: u64, now_ms: u64, staged: usize, min_interval_ms: u64) -> bool {
    staged > 0 && now_ms.saturating_sub(last_attempt_ms) >= min_interval_ms
}

/// Top-level `*.jsonl` batch FILES in `dir` (quarantine/ is a subdir and
/// never matches; directories with jsonl-ish names don't count).
pub(crate) fn staged_batch_count_in(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().map(|t| t.is_file()).unwrap_or(false)
                        && e.path().extension().map(|x| x == "jsonl").unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

/// Open (or reuse) the entity index and run `f`. Opportunistically ingests
/// staged batches first (throttled + cross-process locked — see
/// `ingest_pending_if_due`), so queries and status see fresh data.
pub fn with_index<T>(
    f: impl FnOnce(&bite_index::EntityIndex) -> Result<T, BiteError>,
) -> Result<T, BiteError> {
    let mut guard = INDEX.lock().unwrap();
    if guard.is_none() {
        let index = bite_index::EntityIndex::open(&dir()).map_err(|e| {
            BridgeError::new(
                "index_unavailable",
                format!("cannot open entity index: {e}"),
            )
        })?;
        *guard = Some(index);
    }
    let index = guard.as_ref().unwrap();
    // Ordering: ingest BEFORE f, so queries see fresh data. Never fails the
    // outer call. The detached bite-crawl worker stages batches via atomic
    // rename, so this snapshot of the staging dir is always well-formed;
    // batches that appear mid-ingest are picked up on a later call.
    if !IN_INGEST_PENDING.load(Ordering::SeqCst) {
        ingest_pending_if_due(index);
    }
    let out = f(index);
    // Post-settle FTS refresh LAST: batches that landed after the final
    // mid-crawl ingest must be folded in only after they were ingested
    // (fold-then-ingest would mark a crawl state whose newest rows stay
    // unindexed until the next crawl). No-op within milliseconds when the
    // marker already matches — unindexed rows stay searchable meanwhile.
    refresh_fts_after_settled_crawl(index);
    out
}

/// Opportunistic auto-ingest: cheap gate (30 s throttle FIRST — inside the
/// window we don't even dir-scan the staging dir), then a non-blocking
/// cross-process `.ingest.lock`, then the store's writer lock so LanceDB
/// writes serialize against the spawn/rebuild ingest path and `index_wipe`
/// (which hold ONLY the writer lock). If anything at all goes wrong (lock
/// held, Busy writer, ingest error), stand down silently — the next index
/// touch after the throttle catches up, and `ingest_pending`
/// (spawn/rebuild) remains the authoritative path.
fn ingest_pending_if_due(index: &bite_index::EntityIndex) {
    if IN_INGEST_PENDING.load(Ordering::SeqCst) {
        return; // spawn/rebuild path is ingesting and wants its own reporting
    }
    let now = now_ms();
    let last = LAST_AUTO_INGEST_MS.load(Ordering::SeqCst);
    // Throttle BEFORE the scan: the staging-dir scan is exactly what the
    // throttle exists to keep out of hot tool loops, so it only runs once
    // the window has elapsed.
    if now.saturating_sub(last) < AUTO_INGEST_THROTTLE_MS {
        return;
    }
    let staged = staged_batch_count_in(&staging());
    if !auto_ingest_due(last, now, staged, AUTO_INGEST_THROTTLE_MS) {
        return;
    }
    LAST_AUTO_INGEST_MS.store(now, Ordering::SeqCst);

    let lock_path = bite_core::config::data_dir().join(".ingest.lock");
    let Ok(lock) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
    else {
        return; // cannot even open the lock file — stand down
    };
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        return; // another bite process is mid-ingest — it will catch up
    }
    // The staging lock only serializes auto-ingests against EACH OTHER.
    // The spawn/rebuild ingest path and `index_wipe` hold only the store's
    // writer lock, so without taking it too, auto-ingest could run
    // concurrent LanceDB merge-inserts against them (spurious
    // index_ingest_failed / wipe failures). Busy → the explicit path owns
    // the table right now; stand down silently and let a later touch retry.
    let Ok(_writer) = index.lock_writer() else {
        return;
    };
    let _ = bite_index::ingest_staged(index, &staging()).map_err(|e| {
        eprintln!("bite auto-ingest error: {e}");
        record_index_error("auto-ingest", &e.to_string());
    });
    let _ = fs2::FileExt::unlock(&lock);
    // writer lock still held here (guard `_writer` lives to fn end), so the
    // optimize serializes against concurrent ingests
    refresh_fts_if_crawl_settled(index);
    crate::jobs::set_pending_batches(pending_batches());
}

/// Record a background index failure for `index_status`'s `last_errors`.
/// Consecutive identical entries are not stacked (a hot loop would spam the
/// same line) and the list is capped — an agent needs the recent tail, not
/// a full log.
fn record_index_error(source: &str, err: &str) {
    let mut errs = LAST_INDEX_ERRORS.lock().unwrap();
    let entry = format!("{source}: {err}");
    if errs.last().map(|l| l.as_str()) == Some(entry.as_str()) {
        return;
    }
    errs.push(entry);
    let overflow = errs.len().saturating_sub(LAST_ERRORS_MAX);
    if overflow > 0 {
        errs.drain(0..overflow);
    }
}

/// Raw `crawl-state.json` text — the signature for "which crawl state has
/// been folded into the FTS index". None = no crawl ever recorded state.
fn crawl_state_raw() -> Option<String> {
    std::fs::read_to_string(bite_core::config::data_dir().join("crawl-state.json")).ok()
}

/// Parsed `state` field of raw crawl-state text ("unknown" when unparseable).
fn crawl_state_of(raw: &str) -> String {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(String::from))
        .unwrap_or_else(|| "unknown".to_string())
}

fn fts_refresh_marker_path() -> std::path::PathBuf {
    bite_core::config::data_dir().join("fts-refresh.json")
}

/// Pure gate for the post-settle FTS refresh (see
/// `refresh_fts_if_crawl_settled` for why it must not run mid-crawl).
fn fts_refresh_settled(state: &str, crawler_alive: bool) -> bool {
    if crawler_alive {
        return false;
    }
    // never_run/unknown = no (readable) crawl ever recorded state — there
    // is no crawl whose rows could be sitting unindexed
    !matches!(state, "never_run" | "unknown")
}

/// Is there a settled crawl state whose rows have not been folded into the
/// FTS index yet? Cheap: two small file reads, plus a pid check only when
/// the crawl-state text actually differs from the marker. Read-only — uses
/// the TTL-cached liveness probe.
fn fts_refresh_pending() -> bool {
    let Some(raw) = crawl_state_raw() else {
        return false;
    };
    let marker = std::fs::read_to_string(fts_refresh_marker_path()).unwrap_or_default();
    fts_refresh_due(&raw, &marker, crawl_running_cached())
}

/// Pure form of `fts_refresh_pending` (raw crawl-state text vs marker text).
fn fts_refresh_due(raw: &str, marker: &str, crawler_alive: bool) -> bool {
    if raw == marker {
        return false; // this exact crawl state is already folded in
    }
    fts_refresh_settled(&crawl_state_of(raw), crawler_alive)
}

/// Fold rows written since the last FTS refresh into the index — exactly
/// once per settled crawl state. `EntityIndex::refresh_fts` runs LanceDB
/// `optimize(All)`, which not only adds unindexed rows to the FTS index but
/// also compacts fragments and prunes old dataset versions: running it
/// mid-crawl (the 30 s auto-ingest ticks, the per-batch `job_progress`
/// ingests) would re-compact a table the crawler is still appending to and
/// churn dataset versions under readers — hence the not-while-running gate
/// and the marker that makes it once per crawl state, not once per ingest.
/// A failed optimize is logged and NOT marked: unindexed rows remain
/// searchable (LanceDB FTS scans indexed and unindexed rows in parallel),
/// only slower, so the refresh retries on a later index touch instead of
/// failing the tool call that carried the ingest.
/// Caller must hold the store's writer lock (both ingest paths do).
fn refresh_fts_if_crawl_settled(index: &bite_index::EntityIndex) {
    let Some(raw) = crawl_state_raw() else {
        return;
    };
    let marker = std::fs::read_to_string(fts_refresh_marker_path()).unwrap_or_default();
    if !fts_refresh_due(&raw, &marker, crawl_running_cached()) {
        return;
    }
    match index.refresh_fts() {
        Ok(()) => {
            // mark exactly the state we optimized for — if a new crawl
            // started meanwhile, its state text differs and stays pending
            std::fs::write(fts_refresh_marker_path(), raw).ok();
        }
        Err(e) => {
            eprintln!("bite-index: post-crawl FTS refresh failed: {e}");
            record_index_error("fts-refresh", &e.to_string());
        }
    }
}

/// Post-settle FTS refresh for callers NOT holding the writer lock (the
/// `with_index` tool path): gate on the cheap checks first, then take the
/// writer lock so the optimize serializes against concurrent ingests —
/// Busy just means another bite process owns the table right now and will
/// get its own chance.
fn refresh_fts_after_settled_crawl(index: &bite_index::EntityIndex) {
    if !fts_refresh_pending() {
        return;
    }
    let Ok(_writer) = index.lock_writer() else {
        return;
    };
    refresh_fts_if_crawl_settled(index);
}

/// Ingest any staged JSONL batches. Cheap when nothing is pending.
pub fn ingest_pending() -> Result<Value, BiteError> {
    let _pending = IngestPendingGuard::acquire();
    let staging = staging();
    with_index(|index| {
        // writer lock is per-ingest: a busy lock just means another bite
        // process is ingesting — skip and let the next poll pick it up
        match index.lock_writer() {
            Err(bite_index::LockError::Busy) => {
                crate::jobs::set_pending_batches(pending_batches());
                return Ok(
                    json!({ "skipped": "writer busy", "pending_batches": pending_batches() }),
                );
            }
            Err(e) => {
                return Err(BiteError::from(BridgeError::new(
                    "index_lock_error",
                    e.to_string(),
                )));
            }
            Ok(_guard) => {}
        }
        let (batches, rows, quarantined) = bite_index::ingest_staged(index, &staging)
            .map_err(|e| BridgeError::new("index_ingest_failed", e.to_string()))?;
        if batches > 0 {
            refresh_fts_if_crawl_settled(index);
        }
        crate::jobs::set_pending_batches(pending_batches());
        Ok(json!({
            "batches_ingested": batches,
            "rows": rows,
            "quarantined": quarantined,
        }))
    })
}

/// Top-level batch files — the ONLY ones auto-ingest reads (`ingest_staged`
/// scans the staging dir's top level; `quarantine/` is never ingested).
fn staged_batches() -> usize {
    staged_batch_count_in(&staging())
}

/// Quarantined (poison) batches — never auto-ingested; operators must
/// inspect them.
fn quarantined_batches() -> usize {
    staged_batch_count_in(&staging().join("quarantine"))
}

/// Batches waiting for ingest — including quarantined ones, so operators
/// can see stuck files from index_status instead of discovering them by
/// rummaging through the data dir.
fn pending_batches() -> usize {
    staged_batches() + quarantined_batches()
}

/// Actionable, agent-relayable hint for `index_status`. None when healthy —
/// don't noise up healthy output. `staged` counts TOP-LEVEL batch files
/// (the only ones auto-ingest reads — the hint must not claim automatic
/// ingestion for quarantined batches it never touches); `quarantined`
/// counts quarantine/ poison batches. `failures` is the crawl-state
/// failure streak (reserved for future backoff wording).
fn status_hint(state: &str, staged: usize, quarantined: usize, _failures: u64) -> Option<String> {
    if state == "failed" {
        Some(
            "Mail indexing stopped: see crawl.window for the reason — call index_rebuild to resume"
                .to_string(),
        )
    } else if staged > 0 {
        let mut hint = format!(
            "{staged} staged batches will be ingested automatically on your next index call"
        );
        if quarantined > 0 {
            hint.push_str(&format!(
                "; {quarantined} quarantined batches need inspection (batches/quarantine/)"
            ));
        }
        Some(hint)
    } else if quarantined > 0 {
        Some(format!(
            "{quarantined} quarantined batches need inspection (batches/quarantine/)"
        ))
    } else {
        None
    }
}

pub fn status() -> Result<Value, BiteError> {
    with_index(|index| {
        let stats = index
            .stats()
            .map_err(|e| BridgeError::new("index_error", e.to_string()))?;
        let staged = staged_batches();
        let quarantined = quarantined_batches();
        let crawl = crawl_state();
        let alive = live_crawler_pid_cached().is_some();
        let jobs = crate::jobs::snapshot();
        let last_errors = LAST_INDEX_ERRORS.lock().unwrap().clone();
        Ok(status_json(
            &stats,
            staged,
            quarantined,
            crawl,
            alive,
            jobs,
            &last_errors,
        ))
    })
}

/// Pure assembly of the `index_status` JSON from already-fetched pieces
/// (unit-testable; `alive` is passed in so no probe runs during tests).
/// `last_errors` surfaces exactly when non-empty — healthy output stays
/// key-stable.
fn status_json(
    stats: &bite_index::IndexStats,
    staged: usize,
    quarantined: usize,
    crawl: Value,
    alive: bool,
    jobs: Value,
    last_errors: &[String],
) -> Value {
    let pending = staged + quarantined;
    let state_str = crawl
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let failures = crawl.get("failures").and_then(|v| v.as_u64()).unwrap_or(0);
    let mut base = json!({
        "total": stats.total,
        "per_app": stats.per_app.iter().map(|(a, n)| json!({"app": a, "rows": n})).collect::<Vec<_>>(),
        "newest_updated_ms": stats.newest_updated_ms,
        "fts_indexed": stats.fts_indexed,
        "pending_batches": pending,
        "crawl": {
            "state": crawl,
            "alive": alive,
        },
        "jobs": jobs,
    });
    if let Some(hint) = status_hint(state_str, staged, quarantined, failures) {
        base["hint"] = json!(hint);
    }
    // background failures are only visible here — the auto-ingest / FTS
    // paths never fail the tool call that carried them
    if !last_errors.is_empty() {
        base["last_errors"] = json!(last_errors);
    }
    base
}

// ── detached crawl process management ──

fn crawl_binary() -> Result<std::path::PathBuf, BiteError> {
    let bin = bite_core::config::data_dir().join("bin").join("bite-crawl");
    if bin.exists() {
        return Ok(bin);
    }
    Err(BiteError::from(BridgeError::new(
        "index_unavailable",
        "bite-crawl binary not installed — run `bite install-helper --force`",
    )))
}

fn pid_alive(pid: i32) -> bool {
    // signal 0 = liveness probe
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn mail_rows_present() -> Option<usize> {
    with_index(|index| {
        index
            .count(Some("app = 'mail'"))
            .map_err(|e| BiteError::from(BridgeError::new("index_error", e.to_string())))
    })
    .ok()
}

fn crawl_state() -> Value {
    let path = bite_core::config::data_dir().join("crawl-state.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or(json!({ "state": "unknown" })),
        Err(_) => json!({ "state": "never_run" }),
    }
}

fn crawl_pid() -> Option<i32> {
    crawl_pid_text()?.parse().ok()
}

/// Raw pidfile text — the cache key for the TTL-cached liveness verdict
/// (a rewritten pidfile means a new worker and must invalidate the cache).
fn crawl_pid_text() -> Option<String> {
    let path = bite_core::config::data_dir().join("crawl.pid");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Does this pid actually belong to a bite-crawl process? Pids get recycled;
/// kill(2)-alive alone would let us "cancel" (or be deadlocked by) some
/// unrelated process that inherited the number.
fn crawl_process_matches(pid: i32) -> bool {
    // absolute paths: PATH may be unset/odd inside service contexts
    for ps in ["/bin/ps", "/usr/bin/ps"] {
        match std::process::Command::new(ps)
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
        {
            Ok(o) => {
                return String::from_utf8_lossy(&o.stdout)
                    .to_lowercase()
                    .contains("bite-crawl")
            }
            Err(_) => continue, // try the next ps location
        }
    }
    // every exec failed — fail SAFE: assume the crawler is alive and keep
    // the pidfile. Treating a live crawler as dead here would delete its
    // pidfile and double-spawn two crawls against Mail.
    true
}

/// Pid from the pidfile only while a live bite-crawl owns it. Stale or
/// recycled entries are removed so they can neither block spawns (permanent
/// already_running) nor get signalled. UNCACHED — this is the fresh probe;
/// spawn/cancel paths depend on that.
fn live_crawler_pid() -> Option<i32> {
    let pid = crawl_pid()?;
    if pid_alive(pid) && crawl_process_matches(pid) {
        return Some(pid);
    }
    let _ = std::fs::remove_file(bite_core::config::data_dir().join("crawl.pid"));
    None
}

/// Pure gate for the cached liveness verdict: usable only while the pidfile
/// text still matches what the verdict was computed from AND the TTL has
/// not elapsed.
fn liveness_cache_usable(
    cached_key: &str,
    cached_at_ms: u64,
    key: &str,
    now_ms: u64,
    ttl_ms: u64,
) -> bool {
    cached_key == key && now_ms.saturating_sub(cached_at_ms) <= ttl_ms
}

/// Injectable core of `live_crawler_pid_cached`: consult `cache` for an
/// alive verdict for `key` still within `ttl_ms`; on cache miss or
/// staleness run `fresh_probe` and cache ONLY an alive (Some) verdict — a
/// dead (None) verdict must never be cached, so stale pidfiles keep
/// getting cleared (that is what re-opens the eager auto-respawn).
fn cached_alive_verdict(
    cache: &Mutex<Option<LivenessHit>>,
    key: &str,
    now_ms: u64,
    ttl_ms: u64,
    fresh_probe: &mut dyn FnMut() -> Option<i32>,
) -> Option<i32> {
    if let Some(hit) = cache.lock().unwrap().as_ref() {
        if liveness_cache_usable(&hit.key, hit.at_ms, key, now_ms, ttl_ms) {
            return Some(hit.pid);
        }
    }
    let pid = fresh_probe()?;
    *cache.lock().unwrap() = Some(LivenessHit {
        key: key.to_string(),
        pid,
        at_ms: now_ms,
    });
    Some(pid)
}

/// Read-only crawler liveness with a short TTL. Status rendering, the
/// live-mail gate and the FTS gates call this many times per tool call (and
/// across rapid successive calls), and each fresh probe is a kill + ps.
/// Only the ALIVE verdict is cached: a not-alive result always goes through
/// the fresh identity-checked probe, so stale pidfiles keep getting cleared
/// (that is what re-opens the eager auto-respawn). Safety-critical paths —
/// `spawn_crawl`'s already_running check, `bulk_spawn`'s busy check,
/// `index_crawl_cancel` — call the uncached `live_crawler_pid()` instead.
fn live_crawler_pid_cached() -> Option<i32> {
    let key = crawl_pid_text()?;
    cached_alive_verdict(
        &LIVENESS_CACHE,
        &key,
        now_ms(),
        CRAWLER_LIVENESS_TTL_MS,
        &mut || live_crawler_pid(),
    )
}

fn crawl_running_cached() -> bool {
    live_crawler_pid_cached().is_some()
}

fn crawl_running() -> bool {
    live_crawler_pid().is_some()
}

/// Spawn the detached crawler. Safe to call when one already runs (returns
/// already_running).
fn spawn_crawl(params: &Value) -> Result<Value, BiteError> {
    let bin = crawl_binary()?;
    if crawl_running() {
        return Ok(json!({ "already_running": true, "pid": crawl_pid() }));
    }

    let mut cmd = std::process::Command::new(&bin);
    if let Some(days) = params.get("window_days").and_then(|v| v.as_i64()) {
        cmd.args(["--window-days", &days.to_string()]);
    }
    if let Some(mailbox) = params.get("mailbox").and_then(|v| v.as_str()) {
        cmd.args(["--mailbox", mailbox]);
    }
    if params
        .get("no_body")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        cmd.arg("--no-body");
    }
    // full: bypass the since-last-completed delta window and replay the
    // entire 365-day schedule (the worker decides; forwarded verbatim)
    if params
        .get("full")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        cmd.arg("--full");
    }
    let child = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| BridgeError::new("index_unavailable", format!("cannot spawn crawler: {e}")))?;
    let pid = child.id();
    std::fs::write(
        bite_core::config::data_dir().join("crawl.pid"),
        pid.to_string(),
    )
    .map_err(|e| BridgeError::new("index_error", e.to_string()))?;
    ingest_pending().ok();
    Ok(json!({
        "started": true,
        "pid": pid,
        "window_days": params.get("window_days").and_then(|v| v.as_i64()).unwrap_or(30),
        "note": "crawler is running detached; poll index_status — batches are ingested as they land"
    }))
}

// ── index-backed search (fast path) ──

/// Upper bound for client-supplied search limits. Each hit embeds a content
/// snippet, so the limit trades response size against latency; 200 keeps a
/// single MCP response well inside frame limits no matter what the client asks.
pub const MAX_SEARCH_LIMIT: u64 = 200;
const DEFAULT_SEARCH_LIMIT: u64 = 20;

fn search_limit(params: &Value) -> usize {
    params
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT) as usize
}

fn search_params_to_query(params: &Value) -> bite_index::SearchQuery {
    let g = |k: &str| params.get(k).and_then(|v| v.as_str()).map(String::from);
    bite_index::SearchQuery {
        text: g("query"),
        app: g("app"),
        container: g("mailbox").or_else(|| g("container")),
        // the MCP param is `unread` (true → unread messages) while the index
        // stores `read`; inverting here was returning READ mail for
        // unread:true — opposite of the live path
        read: params
            .get("unread")
            .and_then(|v| v.as_bool())
            .map(|unread| !unread),
        flagged: params.get("flagged").and_then(|v| v.as_bool()),
        junk: params.get("junk").and_then(|v| v.as_bool()),
        completed: params.get("completed").and_then(|v| v.as_bool()),
        since_ms: params
            .get("since")
            .and_then(|v| v.as_str())
            .and_then(bite_core::dates::parse_to_epoch_ms),
        until_ms: params
            .get("until")
            .and_then(|v| v.as_str())
            .and_then(bite_core::dates::parse_to_epoch_ms),
        limit: search_limit(params),
        participant_contains: None,
        title_contains: g("subject"),
        content_contains: g("body"),
        account: g("account"),
        // `skip_count` opts a filter-only search out of the exact-total
        // COUNT scan (a second full filtered pass) — `count`/`total` comes
        // back null, mirroring what full-text queries already return.
        skip_exact_count: params
            .get("skip_count")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// Mail search served from the entity index (fast path). Returns None when
/// the caller should fall back to the live Apple Events path (index has no
/// Mail rows at all — never crawled). When the index exists but a crawl is
/// still filling it, serves from the index and/or degrades to a SHORT live
/// attempt with instant "indexing in progress" feedback instead of hanging.
pub fn try_mail_search(params: &Value) -> Option<Result<Value, BiteError>> {
    let mail_rows = mail_rows_present()?;
    if mail_rows == 0 {
        // never crawled: kick the crawler once (cooldown-guarded), then let
        // the caller decide — MCP falls back to live with a short budget.
        auto_refresh_if_stale();
        return None;
    }
    let mut q = search_params_to_query(params);
    q.app = Some("mail".into());
    q.text = params
        .get("query")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| q.text.clone());
    // The index's `participants` gains to-recipient addresses as rows are
    // re-crawled, but rows indexed before that change still carry the
    // sender only — serving `to` from the index would silently miss them.
    // Fall back to the live path (which checks toRecipients) until a full
    // reindex (`index_rebuild full: true`) has backfilled recipients.
    if params.get("to").and_then(|v| v.as_str()).is_some() {
        return None;
    }
    // `from` filters on the sender, which IS what the index stores.
    if let Some(from) = params.get("from").and_then(|v| v.as_str()) {
        q.participant_contains = Some(from.to_string());
    }
    Some(with_index(|index| {
        let res = index
            .search(&q)
            .map_err(|e| BridgeError::new("index_error", e.to_string()))?;
        Ok(json!({
            "messages": res.hits,
            "source": "index",
            "count": res.total,
            "truncated": res.total.map(|t| t > res.hits.len()).unwrap_or(false),
            "scanned": res.hits.len(),
            "note": index_search_note(res.total, q.skip_exact_count),
        }))
    }))
}

/// The note must not promise an exact count that isn't there: `total` is
/// `None` both for skipped count scans (`skip_count`) and for full-text
/// queries (page-only — an exact total would need a second full pass).
fn index_search_note(total: Option<usize>, skip_exact_count: bool) -> &'static str {
    match (total, skip_exact_count) {
        (Some(_), _) => "served from the entity index — exact count, no Apple Events",
        (None, true) => {
            "served from the entity index — count skipped (skip_count), no Apple Events"
        }
        (None, false) => "served from the entity index — page only (full-text), no Apple Events",
    }
}

/// Instant, non-blocking response for agents querying Mail while the index
/// is still being built and the live Apple Events path is slow/unavailable.
pub fn not_ready_response(why: &str) -> Value {
    let state = crawl_state();
    json!({
        "source": "not_ready",
        "messages": [],
        "indexing": {
            "state": state.get("state").and_then(|v| v.as_str()).unwrap_or("unknown"),
            "processed": state.get("processed").and_then(|v| v.as_i64()).unwrap_or(0),
            "found": state.get("found").and_then(|v| v.as_i64()).unwrap_or(0),
            "alive": crawl_running_cached(),
        },
        "why": why,
        "hint": "Mail indexing is still in progress. Searches may be slow or empty until it completes — poll index_status, or retry in a few minutes. You can still use mail_messages_get for a specific message id.",
    })
}

/// Unified cross-app search (local tool).
pub fn unified_search(params: &Value) -> Result<Value, BiteError> {
    let q = search_params_to_query(params);
    with_index(|index| {
        let res = index
            .search(&q)
            .map_err(|e| BridgeError::new("index_error", e.to_string()))?;
        Ok(json!({
            "hits": res.hits,
            "total": res.total,
            "note": "search covers whatever has been indexed — run index_rebuild to refresh"
        }))
    })
}

// ── local tools ──

/// Tools served locally by the control plane (no helper round-trip for the
/// query itself; mail_bulk_* and index_rebuild spawn the detached worker).
pub const LOCAL_TOOLS: &[&str] = &[
    "index_rebuild",
    "index_status",
    "index_crawl_cancel",
    "index_wipe",
    "index_forget",
    "search",
    "mail_bulk_mark",
    "mail_bulk_move",
    "mail_bulk_delete",
];

pub fn is_local_tool(name: &str) -> bool {
    LOCAL_TOOLS.contains(&name)
}

/// Live Mail tools that read Mail one Apple Event at a time — these are the
/// ones that hang while Mail is saturated or a crawl is hammering it.
pub const LIVE_MAIL_TOOLS: &[&str] = &[
    "mail_accounts",
    "mail_mailboxes_list",
    "mail_messages_search",
    "mail_message_get",
    "mail_reply",
    "mail_forward",
    "mail_move",
    "mail_mark",
    "mail_delete",
    "mail_attachment_save",
];

pub fn is_live_mail_tool(name: &str) -> bool {
    LIVE_MAIL_TOOLS.contains(&name)
}

/// Returns Some(deferred-response) when a live Mail call should get instant
/// feedback instead of blocking behind a slow/unready path. None → run live.
pub fn gate_live_mail(name: &str, params: &Value) -> Option<Value> {
    if !is_live_mail_tool(name) {
        return None;
    }
    if params
        .get("force_live")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return None; // caller accepts the slow live price
    }
    let rows = mail_rows_present().unwrap_or(0);
    if rows > 0 && !crawl_running_cached() {
        return None; // index ready, Mail calm — run live normally
    }
    let state = crawl_state();
    Some(json!({
        "deferred": true,
        "tool": name,
        "reason": if rows == 0 { "index_not_ready" } else { "indexing_in_progress" },
        "indexing": {
            "state": state.get("state").and_then(|v| v.as_str()).unwrap_or("unknown"),
            "processed": state.get("processed").and_then(|v| v.as_i64()).unwrap_or(0),
            "found": state.get("found").and_then(|v| v.as_i64()).unwrap_or(0),
        },
        "hint": "Mail indexing is still in progress — live Mail queries compete with it and may take minutes. Either retry soon (the index will serve this instantly), or re-call with force_live: true to accept a slow live query.",
    }))
}

/// Mail search fast path — returns Some(result) when served from the index
/// or when an instant not-ready response should be given; None means fall
/// back to the normal live helper call with the standard timeout.
pub fn try_search_local(params: &Value) -> Option<Result<Value, BiteError>> {
    auto_refresh_if_stale();
    try_mail_search(params)
}

/// Self-healing: if the index is stale/never built and no crawler runs,
/// respawn it — at most once per 15 minutes (cooldown), from any process.
fn auto_refresh_if_stale() {
    const COOLDOWN_MS: u64 = 15 * 60 * 1000;
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_SPAWN_MS.load(std::sync::atomic::Ordering::SeqCst);
    if now_ms.saturating_sub(last) < COOLDOWN_MS {
        return;
    }
    LAST_SPAWN_MS.store(now_ms, std::sync::atomic::Ordering::SeqCst);

    let state = crawl_state();
    let state_str = state
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("never_run");
    let failures = state.get("failures").and_then(|v| v.as_u64()).unwrap_or(0);
    // cached liveness: this is only the early stand-down check — the
    // authoritative already_running check inside spawn_crawl is always
    // fresh, so a stale "alive" can delay a respawn by the TTL at most and
    // can never double-spawn.
    let is_running = state_str == "running" && crawl_running_cached();
    if is_running {
        return;
    }
    let hours: u64 = {
        let cfg = bite_core::config::Config::cached();
        cfg.auto_refresh_hours.unwrap_or(24)
    };
    let stale = state
        .get("updated_at")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| {
            let t_ms = t.timestamp_millis() as u64;
            now_ms.saturating_sub(t_ms)
        })
        .unwrap_or(u64::MAX);
    // never_run → immediately useful; failed → retry (but a crash loop
    // backs off to 24 h staleness after 5 consecutive failures — the
    // LAST_SPAWN cooldown is per-process and can't bound fresh CLIs);
    // cancelled is user intent — never eagerly respawned.
    if respawn_eligible(state_str, stale, hours, failures) {
        let _ = spawn_crawl(&json!({}));
    }
}

/// Auto-refresh decision. `age_ms` is the crawl-state updated_at age,
/// `failures` the consecutive-failure counter persisted by the crawler
/// (additive contract — absent means 0).
fn respawn_eligible(state: &str, age_ms: u64, threshold_hours: u64, failures: u64) -> bool {
    state == "never_run"
        || (state == "failed" && failures < 5)
        || stale_after_ms(age_ms, threshold_hours)
}

/// `updated_at` age in MILLISECONDS vs a refresh threshold in hours.
/// Keep the units explicit: an epoch-ms ÷ 3600 here once fired auto-refresh
/// ~86 s in instead of after 24 h.
fn stale_after_ms(age_ms: u64, threshold_hours: u64) -> bool {
    age_ms / 3_600_000 >= threshold_hours
}

// ── tool entry points ──

fn rebuild(params: &Value, handle: &mut BridgeHandle) -> Result<Value, BiteError> {
    // warm the helper so the TCC identity is established for future Mail ops
    let _ = handle.get()?;
    spawn_crawl(params)
}

fn cancel(handle: Option<&mut BridgeHandle>) -> Result<Value, BiteError> {
    if let Some(h) = handle {
        let _ = h.get()?;
    }
    match live_crawler_pid() {
        Some(pid) => {
            let ok = std::process::Command::new("kill")
                .args([&pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                // wait out the graceful cancel: the Swift handler waits up
                // to ~2 s (cancel-observed semaphore + final flush + state
                // write) — a 300 ms check misreported normal exits as
                // "process survived". Poll up to ~3 s total.
                let mut survived = true;
                for _ in 0..12 {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    if live_crawler_pid() != Some(pid) {
                        survived = false;
                        break;
                    }
                }
                if survived {
                    return Ok(json!({
                        "cancelled": false,
                        "pid": pid,
                        "note": "process survived SIGTERM — inspect manually",
                    }));
                }
                let _ = std::fs::remove_file(bite_core::config::data_dir().join("crawl.pid"));
            }
            Ok(json!({ "cancelled": ok, "pid": pid }))
        }
        None => Ok(json!({ "cancelled": false, "note": "no running crawler" })),
    }
}

pub fn wipe(confirm: bool) -> Result<Value, BiteError> {
    if !confirm {
        return Ok(json!({
            "would_wipe": dir().display().to_string(),
            "confirm_required": true,
        }));
    }
    with_index(|index| {
        index
            .wipe()
            .map_err(|e| BridgeError::new("index_error", e.to_string()))?;
        let _ = std::fs::remove_dir_all(staging());
        // crawl-state.json too: it carries the delta watermark
        // (mail_completed_at). A wiped index must never look "already
        // crawled up to <recent>" to the next run — that would skip the
        // backfill entirely on a delta and leave the fresh index with a
        // single recent window. Stale crawl.pid is also gone so nothing
        // references the dead run.
        let _ = std::fs::remove_file(bite_core::config::data_dir().join("crawl-state.json"));
        let _ = std::fs::remove_file(bite_core::config::data_dir().join("crawl.pid"));
        Ok(json!({ "wiped": true }))
    })
}

/// SQL filter for the scoped purge. Reuses the store's single-quote escape so
/// a container like `O'Brien` can never break out of the literal.
fn forget_filter(app: Option<&str>, container: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(a) = app {
        parts.push(format!("app = {}", bite_index::store::sql_str(a)));
    }
    if let Some(c) = container {
        parts.push(format!("container = {}", bite_index::store::sql_str(c)));
    }
    parts.join(" AND ")
}

/// Scoped purge of index rows (app and/or container) — the LOCAL copy only;
/// Apple data is never touched. Full reset stays on `index_wipe` (which also
/// clears the delta watermark): an unscoped forget is refused so a bare
/// `confirm:true` can't nuke the index without the wipe-shaped response.
///
/// Reset semantics, not retention: the delta watermark survives a scoped
/// forget, so a delta crawl does NOT resurrect purged rows — but any full
/// crawl (`index_rebuild full:true`, `index_wipe` + rebuild, or the 365-day
/// backfill schedule) re-imports them from Apple. Documented as such in the
/// tool description and PRIVACY.md.
pub fn forget(params: &Value) -> Result<Value, BiteError> {
    // Empty strings count as absent: a JSON `"app": ""` must be refused like
    // a missing one, not become a filter matching nothing-but-still-scanned.
    let app = params
        .get("app")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let container = params
        .get("container")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let confirm = params
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if app.is_none() && container.is_none() {
        return Ok(json!({
            "error": "index_forget requires a scope: pass app and/or container (exact names). For a complete reset use index_wipe (CLI: bite index wipe), which also clears the crawl watermark.",
        }));
    }
    let filter = forget_filter(app.as_deref(), container.as_deref());
    with_index(|index| {
        let scope = json!({ "app": app, "container": container });
        if !confirm {
            let n = index
                .count(Some(&filter))
                .map_err(|e| BridgeError::new("index_error", e.to_string()))?;
            return Ok(json!({
                "would_delete": n,
                "scope": scope,
                "filter": filter,
                "confirm_required": true,
            }));
        }
        // Count first: delete_where reports nothing, and the caller deserves
        // to know how many rows actually left.
        //
        // Writer lock around delete + compaction: this is a WRITE, and the
        // process-wide INDEX mutex is not a write lock (once the lock-scope
        // change lands it only guards the lazy open). wipe/ingest serialize
        // on the store's cross-process `.writer.lock` — a purge must too, or
        // a concurrent auto-ingest (this or another process) can interleave
        // merge-inserts with the delete. Busy is an explicit error, not a
        // silent skip: user-initiated destructive call, deserves a retry.
        let _writer = index.lock_writer().map_err(|e| {
            BridgeError::new(
                "index_lock_error",
                format!(
                    "index is being written by another bite process — retry index_forget shortly ({e})"
                ),
            )
        })?;
        let n = index
            .count(Some(&filter))
            .map_err(|e| BridgeError::new("index_error", e.to_string()))?;
        index
            .delete_where(&filter)
            .map_err(|e| BridgeError::new("index_error", e.to_string()))?;
        // Deleted rows leave dead FTS fragments that still hold plaintext
        // until compaction — fold them away immediately instead of waiting
        // for the next crawl-settled optimize.
        index
            .refresh_fts()
            .map_err(|e| BridgeError::new("index_error", e.to_string()))?;
        Ok(json!({
            "deleted": n,
            "scope": scope,
            "note": "local index only — Apple data untouched; a full re-crawl re-imports these rows (reset, not retention)",
        }))
    })
}

/// Execute a local index tool. Returns None when `name` is not local.
pub fn run_local(
    name: &str,
    params: &Value,
    handle: Option<&mut BridgeHandle>,
) -> Option<Result<Value, BiteError>> {
    match name {
        "index_status" => {
            let crawl = handle.and_then(|h| {
                h.get()
                    .ok()
                    .and_then(|b| b.call("index.crawl_status", &json!({})).ok())
            });
            let mut base = match status() {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            if let Some(c) = crawl {
                base["crawl_helper"] = c;
            }
            Some(Ok(base))
        }
        "index_rebuild" => match handle {
            Some(h) => Some(rebuild(params, h)),
            None => Some(Err(cli_error("helper handle required"))),
        },
        "index_crawl_cancel" => Some(cancel(handle)),
        "index_wipe" => Some(wipe(
            params
                .get("confirm")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        )),
        "index_forget" => Some(forget(params)),
        "search" => Some(unified_search(params)),
        "mail_bulk_mark" | "mail_bulk_move" | "mail_bulk_delete" => Some(bulk_spawn(name, params)),
        _ => None,
    }
}

/// Like run_local but for known-local tools (unwraps the Option).
pub fn run_local_unwrapped(
    name: &str,
    params: &Value,
    handle: Option<&mut BridgeHandle>,
) -> Result<Value, BiteError> {
    match run_local(name, params, handle) {
        Some(r) => r,
        None => Err(cli_error(format!("'{name}' is not a local tool"))),
    }
}

/// Note on a spawned bulk job. Points at the REAL status tool: agents poll
/// `index_status` (there is no `job_status` tool — the old note sent them
/// polling a name that does not exist).
const BULK_JOB_NOTE: &str =
    "running detached — poll index_status; Mail does the iteration internally";

/// bulk tools run in the detached crawler process (its AE path is the only
/// fast one); mark/move/delete map to bite-crawl --bulk arguments.
fn bulk_spawn(name: &str, params: &Value) -> Result<Value, BiteError> {
    let op = match name {
        "mail_bulk_mark" => "mark",
        "mail_bulk_move" => "move",
        "mail_bulk_delete" => "delete",
        _ => return Err(cli_error("unknown bulk op")),
    };
    let mailbox = params
        .get("mailbox")
        .and_then(|v| v.as_str())
        .unwrap_or("INBOX")
        .to_string();
    let confirm = params
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let to_mailbox = params
        .get("to_mailbox")
        .and_then(|v| v.as_str())
        .map(String::from);
    let unread = params.get("unread").and_then(|v| v.as_bool());
    let read = params.get("read").and_then(|v| v.as_bool());
    let flagged = params.get("flagged").and_then(|v| v.as_bool());
    let junk = params.get("junk").and_then(|v| v.as_bool());
    let older = params.get("older_than_days").and_then(|v| v.as_i64());
    if let Some(d) = older {
        if d <= 0 {
            return Err(BiteError::from(BridgeError::new(
                "invalid_params",
                "older_than_days must be a positive integer",
            )));
        }
    }
    if op == "mark" && read.is_none() && flagged.is_none() && junk.is_none() {
        return Err(BiteError::from(BridgeError::new(
            "invalid_params",
            "bulk mark needs at least one of read/flagged/junk to set",
        )));
    }

    let selection_label = {
        let mut parts: Vec<String> = Vec::new();
        if let Some(u) = unread {
            parts.push(if u { "unread".into() } else { "read".into() });
        }
        if let Some(d) = older {
            parts.push(format!("older than {d} days"));
        }
        if op == "mark" {
            if let Some(v) = read {
                parts.push(format!("set read={v}"));
            }
            if let Some(f) = flagged {
                parts.push(format!("set flagged={f}"));
            }
            if let Some(j) = junk {
                parts.push(format!("set junk={j}"));
            }
        }
        if parts.is_empty() {
            "all messages".to_string()
        } else {
            parts.join(", ")
        }
    };

    if !confirm {
        return Ok(json!({
            "operation": op,
            "mailbox": mailbox,
            "selection": selection_label,
            "to_mailbox": to_mailbox,
            "confirm_required": true,
            "note": "review this preview, then re-call with confirm: true to execute",
        }));
    }

    let bin = crawl_binary()?;
    if crawl_running() {
        return Err(BiteError::from(BridgeError::new(
            "already_running",
            "another crawl/bulk job is running — cancel it first or wait",
        )));
    }

    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["--bulk-op", op, "--bulk-mailbox", &mailbox]);
    if let Some(to) = to_mailbox {
        cmd.args(["--to-mailbox", to.as_str()]);
    }
    if let Some(account) = params.get("account").and_then(|v| v.as_str()) {
        cmd.args(["--bulk-account", account]);
    }
    // `unread` is a SELECTION filter in both directions: true → unread
    // messages, false → already-read messages (an explicit false must not
    // silently widen to "all messages"). It never derives a setter.
    if let Some(u) = unread {
        cmd.args(["--unread", if u { "true" } else { "false" }]);
    }
    if let Some(d) = older {
        cmd.args(["--older-than-days", &d.to_string()]);
    }
    if op == "mark" {
        // forward the setters verbatim from the documented params — these
        // were parsed into the preview label only before, so bulk-mark
        // silently no-op'd
        if let Some(v) = read {
            let s = v.to_string();
            cmd.args(["--set-read", s.as_str()]);
        }
        if let Some(f) = flagged {
            let s = f.to_string();
            cmd.args(["--set-flagged", s.as_str()]);
        }
        if let Some(j) = junk {
            let s = j.to_string();
            cmd.args(["--set-junk", s.as_str()]);
        }
    }
    let child = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| BridgeError::new("index_error", format!("cannot spawn bulk worker: {e}")))?;
    let pid = child.id();
    std::fs::write(
        bite_core::config::data_dir().join("crawl.pid"),
        pid.to_string(),
    )
    .map_err(|e| BridgeError::new("index_error", e.to_string()))?;
    Ok(json!({
        "started": true,
        "job": name,
        "pid": pid,
        "mailbox": mailbox,
        "selection": selection_label,
        "note": BULK_JOB_NOTE
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forget_filter_scopes_and_escapes() {
        assert_eq!(forget_filter(Some("mail"), None), "app = 'mail'");
        assert_eq!(forget_filter(None, Some("INBOX")), "container = 'INBOX'");
        // both scopes AND together
        assert_eq!(
            forget_filter(Some("mail"), Some("INBOX")),
            "app = 'mail' AND container = 'INBOX'"
        );
        // single quotes in names must never break out of the SQL literal
        assert_eq!(
            forget_filter(None, Some("O'Brien's")),
            "container = 'O''Brien''s'"
        );
    }

    #[test]
    fn forget_without_scope_is_refused_before_touching_the_index() {
        // no app and no container → refusal (and it must NOT open the index:
        // the call returns before with_index, so this runs on any machine)
        let out = forget(&json!({ "confirm": true })).unwrap();
        assert!(out.get("error").is_some(), "unscoped forget must refuse");
        assert!(
            out["error"].as_str().unwrap().contains("index_wipe"),
            "refusal must point at the full-reset path"
        );
        // a scope of empty strings is as good as no scope — refuse too
        let out = forget(&json!({ "app": "", "container": "" })).unwrap();
        assert!(out.get("error").is_some());
    }

    #[test]
    fn search_limit_defaults_to_20() {
        assert_eq!(search_limit(&json!({})), 20);
        assert_eq!(search_limit(&json!({ "limit": null })), 20);
        assert_eq!(search_limit(&json!({ "limit": "50" })), 20); // non-number ignored
    }

    #[test]
    fn search_limit_clamps_client_supplied_values() {
        // within range passes through
        assert_eq!(search_limit(&json!({ "limit": 1 })), 1);
        assert_eq!(search_limit(&json!({ "limit": 50 })), 50);
        assert_eq!(search_limit(&json!({ "limit": 200 })), 200);
        // above the cap clamps down (was: u64::MAX → usize cast, unbounded scan)
        assert_eq!(search_limit(&json!({ "limit": 100_000 })), 200);
        assert_eq!(search_limit(&json!({ "limit": u64::MAX })), 200);
        // zero floors to 1 (store.query() already floors; keep that contract)
        assert_eq!(search_limit(&json!({ "limit": 0 })), 1);
    }

    #[test]
    fn search_params_map_subject_body_account_to_contains_filters() {
        // subject/body/account used to be dropped entirely — search_params_to_query
        // read them into *_like fields that filter_sql never saw
        let q = search_params_to_query(&json!({
            "subject": "invoice",
            "body": "payment",
            "account": "iCloud",
            "mailbox": "INBOX"
        }));
        assert_eq!(q.title_contains.as_deref(), Some("invoice"));
        assert_eq!(q.content_contains.as_deref(), Some("payment"));
        assert_eq!(q.account.as_deref(), Some("iCloud"));
        assert_eq!(q.container.as_deref(), Some("INBOX"));
        // needles stay literal substrings — no implicit % wrapping
        let q = search_params_to_query(&json!({ "subject": "50%" }));
        assert_eq!(q.title_contains.as_deref(), Some("50%"));
    }

    #[test]
    fn unread_param_maps_to_read_inverted() {
        // unread:true must ask for UNREAD messages (read = false), matching
        // the live path; it used to map straight onto `read` and return
        // read mail — the exact opposite
        let q = search_params_to_query(&json!({ "unread": true }));
        assert_eq!(q.read, Some(false));
        let q = search_params_to_query(&json!({ "unread": false }));
        assert_eq!(q.read, Some(true));
        let q = search_params_to_query(&json!({}));
        assert_eq!(q.read, None);
    }

    #[test]
    fn skip_count_param_maps_to_skip_exact_count() {
        // default keeps the exact-total COUNT scan (current behavior);
        // skip_count:true opts out of it — total/count comes back null
        let q = search_params_to_query(&json!({}));
        assert!(!q.skip_exact_count);
        let q = search_params_to_query(&json!({ "unread": true }));
        assert!(!q.skip_exact_count);
        let q = search_params_to_query(&json!({ "skip_count": true }));
        assert!(q.skip_exact_count);
        let q = search_params_to_query(&json!({ "skip_count": false }));
        assert!(!q.skip_exact_count);
    }

    #[test]
    fn index_search_note_never_claims_an_exact_count_that_is_absent() {
        // exact total → exact-count note (skip_count irrelevant: a text
        // query never has one)
        assert_eq!(
            index_search_note(Some(42), false),
            "served from the entity index — exact count, no Apple Events"
        );
        // count skipped by request → says so
        assert_eq!(
            index_search_note(None, true),
            "served from the entity index — count skipped (skip_count), no Apple Events"
        );
        // None WITHOUT skip_count = full-text page — must not blame
        // skip_count (it was never passed) nor promise exactness
        assert_eq!(
            index_search_note(None, false),
            "served from the entity index — page only (full-text), no Apple Events"
        );
    }

    #[test]
    fn staleness_units_are_milliseconds() {
        // 90 s of age must NOT read as 24 h stale (the old ÷3600 bug fired
        // auto-refresh ~86 s after every crawl)
        assert!(!stale_after_ms(90_000, 24));
        // exactly 24 h old is stale for a 24 h threshold; 23 h is not
        assert!(stale_after_ms(24 * 3_600_000, 24));
        assert!(!stale_after_ms(23 * 3_600_000, 24));
        // never-run sentinel (u64::MAX) is always stale
        assert!(stale_after_ms(u64::MAX, 24));
    }

    #[test]
    fn auto_refresh_eligibility() {
        const H24: u64 = 24;
        const DAY_MS: u64 = 24 * 3_600_000;
        // never_run: always eligible
        assert!(respawn_eligible("never_run", 0, H24, 0));
        assert!(respawn_eligible("never_run", u64::MAX, H24, 9));
        // failed: eligible while the consecutive-failure count is low —
        // even with a fresh updated_at (its freshness must not shelter a
        // dead crawl) — but a crash loop backs off to staleness at 5
        assert!(respawn_eligible("failed", 5_000, H24, 0));
        assert!(respawn_eligible("failed", 5_000, H24, 4));
        assert!(!respawn_eligible("failed", 5_000, H24, 5));
        assert!(!respawn_eligible("failed", 5_000, H24, 9));
        // …and a stale failed state is eligible regardless of the counter
        assert!(respawn_eligible("failed", DAY_MS, H24, 9));
        // missing counter (0) keeps legacy state files working
        assert!(respawn_eligible("failed", 5_000, H24, 0));
        // cancelled is user intent: never eagerly respawned…
        assert!(!respawn_eligible("cancelled", 5_000, H24, 0));
        // …only via ordinary staleness
        assert!(respawn_eligible("cancelled", DAY_MS, H24, 0));
        // done: ordinary staleness only
        assert!(!respawn_eligible("done", 5_000, H24, 3));
        assert!(respawn_eligible("done", DAY_MS, H24, 3));
        assert!(!respawn_eligible("running", 0, H24, 0));
    }

    #[test]
    fn auto_ingest_due_gate() {
        const THROTTLE: u64 = 30_000;
        // nothing staged → never due, however stale the last attempt
        assert!(!auto_ingest_due(0, u64::MAX, 0, THROTTLE));
        // staged + first-ever attempt (last = 0) → due immediately
        assert!(auto_ingest_due(0, 100_000, 3, THROTTLE));
        // within the throttle window → not due
        assert!(!auto_ingest_due(
            100_000,
            100_000 + THROTTLE - 1,
            3,
            THROTTLE
        ));
        // exactly at the interval → due
        assert!(auto_ingest_due(100_000, 100_000 + THROTTLE, 3, THROTTLE));
        // long overdue → due
        assert!(auto_ingest_due(
            100_000,
            100_000 + 10 * THROTTLE,
            3,
            THROTTLE
        ));
        // saturating math: a huge last-attempt must not panic/underflow
        assert!(!auto_ingest_due(u64::MAX, 0, 3, THROTTLE));
    }

    #[test]
    fn fts_refresh_settled_gate() {
        // live crawler: never due, whatever the state
        assert!(!fts_refresh_settled("running", true));
        assert!(!fts_refresh_settled("done", true));
        // no crawl ever recorded state (missing file): nothing to fold in
        assert!(!fts_refresh_settled("never_run", false));
        assert!(!fts_refresh_settled("unknown", false));
        // terminal states with no live crawler: due exactly once
        assert!(fts_refresh_settled("done", false));
        assert!(fts_refresh_settled("failed", false));
        assert!(fts_refresh_settled("cancelled", false));
        // running state with a DEAD crawler (crashed worker, stale pid):
        // rows may sit unindexed — still due
        assert!(fts_refresh_settled("running", false));
    }

    #[test]
    fn fts_refresh_due_marker_matching() {
        let done = r#"{"state":"done","processed":10}"#;
        // marker equals raw state → already folded in, not due
        assert!(!fts_refresh_due(done, done, false));
        // any other marker (older crawl, or never written) → due
        assert!(fts_refresh_due(done, "", false));
        assert!(fts_refresh_due(done, r#"{"state":"running"}"#, false));
        // unparseable crawl state text behaves as unknown → not due
        assert!(!fts_refresh_due("garbage", "", false));
        // and the alive gate still wins over a differing marker
        assert!(!fts_refresh_due(done, "", true));
    }

    #[test]
    fn crawl_state_of_parses_state_field() {
        assert_eq!(
            crawl_state_of(r#"{"state":"done","processed":3}"#),
            "done".to_string()
        );
        assert_eq!(crawl_state_of("garbage"), "unknown".to_string());
        assert_eq!(crawl_state_of(r#"{"no_state":1}"#), "unknown".to_string());
    }

    #[test]
    fn status_hint_table() {
        let failed =
            "Mail indexing stopped: see crawl.window for the reason — call index_rebuild to resume";
        // failed wins over pending, failures are currently informational only
        assert_eq!(status_hint("failed", 19, 0, 5).as_deref(), Some(failed));
        assert_eq!(status_hint("failed", 0, 3, 0).as_deref(), Some(failed));
        // staged batches: agent-relayable count; auto-ingest only fires on
        // an index touch, so the wording says so
        assert_eq!(
            status_hint("running", 19, 0, 0).as_deref(),
            Some("19 staged batches will be ingested automatically on your next index call")
        );
        assert_eq!(
            status_hint("done", 1, 0, 0).as_deref(),
            Some("1 staged batches will be ingested automatically on your next index call")
        );
        // quarantined batches ride along when staged batches exist…
        assert_eq!(
            status_hint("running", 2, 3, 0).as_deref(),
            Some(
                "2 staged batches will be ingested automatically on your next index call; \
                 3 quarantined batches need inspection (batches/quarantine/)"
            )
        );
        // …and get an inspection-only hint when nothing else is pending
        // (accurate: quarantine is NEVER auto-ingested)
        assert_eq!(
            status_hint("done", 0, 4, 0).as_deref(),
            Some("4 quarantined batches need inspection (batches/quarantine/)")
        );
        // healthy → no hint at all (don't noise up healthy output)
        assert_eq!(status_hint("done", 0, 0, 0), None);
        assert_eq!(status_hint("running", 0, 0, 0), None);
        assert_eq!(status_hint("cancelled", 0, 0, 0), None);
        assert_eq!(status_hint("partial", 0, 0, 0), None);
        assert_eq!(status_hint("waiting_mail", 0, 0, 0), None);
    }

    #[test]
    fn bulk_job_note_points_at_the_real_status_tool() {
        // agents poll index_status; there is no job_status tool — the old
        // note sent them polling a name that does not exist
        assert!(BULK_JOB_NOTE.contains("index_status"));
        assert!(!BULK_JOB_NOTE.contains("job_status"));
    }

    #[test]
    fn cached_alive_verdict_caches_only_alive_within_ttl() {
        use std::cell::Cell;
        let cache: Mutex<Option<LivenessHit>> = Mutex::new(None);
        let calls = Cell::new(0u32);
        let mut alive_probe = || {
            calls.set(calls.get() + 1);
            Some(7)
        };
        // first call: cache miss → probe runs
        assert_eq!(
            cached_alive_verdict(&cache, "1234", 1_000, 5_000, &mut alive_probe),
            Some(7)
        );
        assert_eq!(calls.get(), 1);
        // same pidfile text within the TTL → served from cache, NO probe
        assert_eq!(
            cached_alive_verdict(&cache, "1234", 1_000 + 4_999, 5_000, &mut alive_probe),
            Some(7)
        );
        assert_eq!(calls.get(), 1);
        // past the TTL → stale, re-probes
        assert_eq!(
            cached_alive_verdict(&cache, "1234", 1_000 + 5_001, 5_000, &mut alive_probe),
            Some(7)
        );
        assert_eq!(calls.get(), 2);
        // different pidfile text (new worker pid) → the cache is never
        // trusted, whatever the age
        assert_eq!(
            cached_alive_verdict(&cache, "5678", 1_000 + 5_001, 5_000, &mut alive_probe),
            Some(7)
        );
        assert_eq!(calls.get(), 3);

        // a dead (None) verdict is NEVER cached: the probe runs again on
        // the very next call so stale pidfiles keep getting cleared
        let dead_calls = Cell::new(0u32);
        let mut dead_probe = || {
            dead_calls.set(dead_calls.get() + 1);
            None
        };
        assert_eq!(
            cached_alive_verdict(&cache, "1234", 0, 5_000, &mut dead_probe),
            None
        );
        assert_eq!(
            cached_alive_verdict(&cache, "1234", 1, 5_000, &mut dead_probe),
            None
        );
        assert_eq!(dead_calls.get(), 2, "a None verdict must not be cached");
    }

    #[test]
    fn spawn_cancel_bulk_paths_probe_fresh_not_cached() {
        // Tripwire pinning the safety constraint the doc comments state
        // (see `live_crawler_pid_cached`): crawl spawn, crawl cancel and
        // bulk spawn must decide on the UNCACHED identity-checked probe —
        // a TTL-stale "alive" verdict there could double-spawn crawlers
        // against Mail or signal a recycled pid. All probe fns are
        // private, so this pins the SOURCE text: swapping any of these
        // call sites to the cached variant compiles and passes every
        // behavioral test, but must fail here.
        let src = include_str!("index.rs");

        // naive body slice: from `fn <name>(` to the earliest of the next
        // top-level `\nfn `, `\n///` doc comment, or the test module —
        // the last bound keeps `fn bulk_spawn`'s slice (nothing follows
        // it but `#[cfg(test)]`) from swallowing this test's own
        // assertion strings and self-failing
        let body = |marker: &str, known_line: &str| {
            let start = src
                .find(marker)
                .unwrap_or_else(|| panic!("tripwire anchor lost: {marker}"));
            let rest = &src[start..];
            let end = [
                rest.find("\nfn "),
                rest.find("\n///"),
                rest.find("\n#[cfg(test)]"),
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(rest.len());
            let slice = &rest[..end];
            assert!(
                !slice.trim().is_empty() && slice.contains(known_line),
                "tripwire slice for {marker} is empty or off-target — \
                 the anchor or known-line drifted"
            );
            slice
        };

        let spawn = body("fn spawn_crawl(", "\"already_running\": true");
        let cancel = body("fn cancel(", "\"no running crawler\"");
        let bulk = body("fn bulk_spawn(", "\"--bulk-op\"");

        for (name, slice) in [
            ("spawn_crawl", spawn),
            ("cancel", cancel),
            ("bulk_spawn", bulk),
        ] {
            assert!(
                slice.contains("crawl_running()") || slice.contains("live_crawler_pid()"),
                "{name} must decide on the UNCACHED liveness probe"
            );
            assert!(
                !slice.contains("crawl_running_cached()")
                    && !slice.contains("live_crawler_pid_cached()"),
                "{name} must not use the TTL-cached liveness probe"
            );
        }
    }

    #[test]
    fn liveness_cache_gate_respects_key_and_ttl() {
        const TTL: u64 = CRAWLER_LIVENESS_TTL_MS;
        // same pidfile text, within the TTL (inclusive) → usable
        assert!(liveness_cache_usable(
            "1234",
            1_000,
            "1234",
            1_000 + TTL,
            TTL
        ));
        // one tick past the TTL → stale, re-probe
        assert!(!liveness_cache_usable(
            "1234",
            1_000,
            "1234",
            1_000 + TTL + 1,
            TTL
        ));
        // pidfile rewritten (new worker pid) → never trusted, whatever the age
        assert!(!liveness_cache_usable("1234", 1_000, "5678", 1_000, TTL));
        assert!(!liveness_cache_usable("1234", 1_000, "5678", 0, TTL));
        // saturating math: elapsed clamps at 0 (never panics, never wraps)
        assert!(liveness_cache_usable(
            "1234",
            u64::MAX,
            "1234",
            u64::MAX,
            TTL
        ));
        // ttl 0: only a verdict taken at this exact instant is usable
        assert!(liveness_cache_usable("1234", 1_000, "1234", 1_000, 0));
        assert!(!liveness_cache_usable("1234", 999, "1234", 1_000, 0));
    }

    #[test]
    fn index_error_record_dedupes_repeats_and_caps_length() {
        // NOTE: global static — tolerates entries left by other tests
        let before = LAST_INDEX_ERRORS.lock().unwrap().len();
        record_index_error("auto-ingest", "boom");
        record_index_error("auto-ingest", "boom"); // identical repeat: no stack-up
        {
            let errs = LAST_INDEX_ERRORS.lock().unwrap();
            assert_eq!(errs.len(), before + 1);
            assert_eq!(errs.last().unwrap(), "auto-ingest: boom");
        }
        // a flood of distinct failures stays capped at the tail
        for i in 0..(LAST_ERRORS_MAX * 2) {
            record_index_error("fts-refresh", &format!("e{i}"));
        }
        let errs = LAST_INDEX_ERRORS.lock().unwrap();
        assert_eq!(errs.len(), LAST_ERRORS_MAX);
        assert!(
            errs.last()
                .unwrap()
                .ends_with(&format!("e{}", LAST_ERRORS_MAX * 2 - 1)),
            "the newest failure survives the cap: {errs:?}"
        );
    }

    #[test]
    fn status_json_surfaces_last_errors_only_when_present() {
        let stats = bite_index::IndexStats {
            total: 3,
            per_app: vec![("mail".to_string(), 3)],
            newest_updated_ms: Some(7),
            fts_indexed: true,
        };
        let crawl = json!({ "state": "done", "failures": 0 });
        let jobs = json!({ "jobs": [], "pending_batches": 0 });

        // empty last_errors → NO last_errors key at all; the rest of the
        // payload (hint, pending counts, crawl alive) is unchanged
        let v = status_json(&stats, 1, 2, crawl.clone(), false, jobs.clone(), &[]);
        assert_eq!(v["total"], 3);
        assert_eq!(v["pending_batches"], 3);
        assert_eq!(v["crawl"]["alive"], false);
        assert!(v["hint"].is_string(), "got: {v}");
        assert!(v.get("last_errors").is_none(), "got: {v}");

        // non-empty → key present with the entries verbatim
        let errs = vec![
            "auto-ingest: boom".to_string(),
            "fts-refresh: e1".to_string(),
        ];
        let v = status_json(&stats, 0, 0, crawl, true, jobs, &errs);
        assert_eq!(v["last_errors"], json!(errs));
        assert_eq!(v["crawl"]["alive"], true);
        // nothing else pending + done state → still no hint, only the errors
        assert!(v.get("hint").is_none(), "got: {v}");
    }

    #[test]
    fn staged_batch_count_scans_top_level_only() {
        let base = tempfile::Builder::new()
            .prefix("bite-test-staging-")
            .tempdir()
            .unwrap();
        let base = base.path();
        let quarantine = base.join("quarantine");
        std::fs::create_dir_all(&quarantine).unwrap();
        // no files yet
        assert_eq!(staged_batch_count_in(base), 0);
        // three top-level batches
        for name in ["a-0.jsonl", "a-1.jsonl", "b-0.jsonl"] {
            std::fs::write(base.join(name), b"{}\n").unwrap();
        }
        // non-jsonl and tmp files don't count
        std::fs::write(base.join("a-0.jsonl.tmp"), b"").unwrap();
        std::fs::write(base.join("notes.txt"), b"").unwrap();
        // quarantine is a subdir: its jsonl files must NOT count here
        std::fs::write(quarantine.join("poison.jsonl"), b"garbage").unwrap();
        assert_eq!(staged_batch_count_in(base), 3);
        // empty subdirectories with jsonl-ish names don't fool the scan
        std::fs::create_dir_all(base.join("weird.jsonl")).unwrap();
        assert_eq!(staged_batch_count_in(base), 3, "directories must not count");
    }
}
