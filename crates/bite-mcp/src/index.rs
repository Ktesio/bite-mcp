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
fn staged_batch_count_in(dir: &std::path::Path) -> usize {
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
    f(index)
}

/// Opportunistic auto-ingest: cheap gate (30 s throttle + staged-batch
/// scan), then a non-blocking cross-process `.ingest.lock` so concurrent
/// bite processes don't pile onto ingest_staged together. If anything at
/// all goes wrong (lock held, Busy writer, ingest error), stand down
/// silently — the next index touch after the throttle catches up, and
/// `ingest_pending` (spawn/rebuild) remains the authoritative path.
fn ingest_pending_if_due(index: &bite_index::EntityIndex) {
    if IN_INGEST_PENDING.load(Ordering::SeqCst) {
        return; // spawn/rebuild path is ingesting and wants its own reporting
    }
    let now = now_ms();
    let last = LAST_AUTO_INGEST_MS.load(Ordering::SeqCst);
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
    let _ = bite_index::ingest_staged(index, &staging())
        .map_err(|e| eprintln!("bite auto-ingest error: {e}"));
    let _ = fs2::FileExt::unlock(&lock);
    crate::jobs::set_pending_batches(pending_batches());
}

/// Ingest any staged JSONL batches. Cheap when nothing is pending.
pub fn ingest_pending() -> Result<Value, BiteError> {
    IN_INGEST_PENDING.store(true, Ordering::SeqCst);
    let staging = staging();
    let result = with_index(|index| {
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
        crate::jobs::set_pending_batches(pending_batches());
        Ok(json!({
            "batches_ingested": batches,
            "rows": rows,
            "quarantined": quarantined,
        }))
    });
    IN_INGEST_PENDING.store(false, Ordering::SeqCst);
    result
}

/// Batches waiting for ingest — including quarantined ones, so operators
/// can see stuck files from index_status instead of discovering them by
/// rummaging through the data dir.
fn pending_batches() -> usize {
    staged_batch_count_in(&staging()) + staged_batch_count_in(&staging().join("quarantine"))
}

/// Actionable, agent-relayable hint for `index_status`. None when healthy —
/// don't noise up healthy output. `failures` is the crawl-state failure
/// streak (reserved for future backoff wording).
fn status_hint(state: &str, pending: usize, _failures: u64) -> Option<String> {
    if state == "failed" {
        Some(
            "Mail indexing stopped: see crawl.window for the reason — call index_rebuild to resume"
                .to_string(),
        )
    } else if pending > 0 {
        Some(format!(
            "{pending} staged batches will be ingested automatically within a minute"
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
        let pending = pending_batches();
        let crawl = crawl_state();
        let state_str = crawl
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let failures = crawl.get("failures").and_then(|v| v.as_u64()).unwrap_or(0);
        let mut base = json!({
            "total": stats.total,
            "per_app": stats.per_app.into_iter().map(|(a, n)| json!({"app": a, "rows": n})).collect::<Vec<_>>(),
            "newest_updated_ms": stats.newest_updated_ms,
            "fts_indexed": stats.fts_indexed,
            "pending_batches": pending,
            "crawl": {
                "state": crawl,
                "alive": live_crawler_pid().is_some(),
            },
            "jobs": crate::jobs::snapshot(),
        });
        if let Some(hint) = status_hint(state_str, pending, failures) {
            base["hint"] = json!(hint);
        }
        Ok(base)
    })
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
    let path = bite_core::config::data_dir().join("crawl.pid");
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
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
/// already_running) nor get signalled.
fn live_crawler_pid() -> Option<i32> {
    let pid = crawl_pid()?;
    if pid_alive(pid) && crawl_process_matches(pid) {
        return Some(pid);
    }
    let _ = std::fs::remove_file(bite_core::config::data_dir().join("crawl.pid"));
    None
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

fn search_params_to_query(params: &Value) -> bite_index::SearchQuery {
    let g = |k: &str| params.get(k).and_then(|v| v.as_str()).map(String::from);
    bite_index::SearchQuery {
        text: g("query"),
        app: g("app"),
        container: g("mailbox").or_else(|| g("container")),
        read: params.get("unread").and_then(|v| v.as_bool()),
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
        limit: params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize,
        participant_like: None,
        title_like: g("subject").map(|s| format!("%{s}%")),
        content_like: g("body").map(|s| format!("%{s}%")),
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
    // from/to → participants LIKE
    if let Some(from) = params.get("from").and_then(|v| v.as_str()) {
        q.participant_like = Some(format!("%{from}%"));
    }
    if let Some(to) = params.get("to").and_then(|v| v.as_str()) {
        q.participant_like = Some(format!("%{to}%"));
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
            "note": "served from the entity index — exact count, no Apple Events",
        }))
    }))
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
            "alive": crawl_running(),
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
    if rows > 0 && !crawl_running() {
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
    let is_running = state_str == "running" && crawl_running();
    if is_running {
        return;
    }
    let hours: u64 = {
        let cfg = bite_core::config::Config::load();
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
        Ok(json!({ "wiped": true }))
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
        "note": "running detached — poll job_status; Mail does the iteration internally"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn status_hint_table() {
        let failed =
            "Mail indexing stopped: see crawl.window for the reason — call index_rebuild to resume";
        // failed wins over pending, failures are currently informational only
        assert_eq!(status_hint("failed", 19, 5).as_deref(), Some(failed));
        assert_eq!(status_hint("failed", 0, 0).as_deref(), Some(failed));
        // pending batches: agent-relayable count
        assert_eq!(
            status_hint("running", 19, 0).as_deref(),
            Some("19 staged batches will be ingested automatically within a minute")
        );
        assert_eq!(
            status_hint("done", 1, 0).as_deref(),
            Some("1 staged batches will be ingested automatically within a minute")
        );
        // healthy → no hint at all (don't noise up healthy output)
        assert_eq!(status_hint("done", 0, 0), None);
        assert_eq!(status_hint("running", 0, 0), None);
        assert_eq!(status_hint("cancelled", 0, 0), None);
        assert_eq!(status_hint("partial", 0, 0), None);
        assert_eq!(status_hint("waiting_mail", 0, 0), None);
    }

    #[test]
    fn staged_batch_count_scans_top_level_only() {
        let base = std::env::temp_dir().join(format!("bite-test-staging-{}", std::process::id()));
        let quarantine = base.join("quarantine");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&quarantine).unwrap();
        // no files yet
        assert_eq!(staged_batch_count_in(&base), 0);
        // three top-level batches
        for name in ["a-0.jsonl", "a-1.jsonl", "b-0.jsonl"] {
            std::fs::write(base.join(name), b"{}\n").unwrap();
        }
        // non-jsonl and tmp files don't count
        std::fs::write(base.join("a-0.jsonl.tmp"), b"").unwrap();
        std::fs::write(base.join("notes.txt"), b"").unwrap();
        // quarantine is a subdir: its jsonl files must NOT count here
        std::fs::write(quarantine.join("poison.jsonl"), b"garbage").unwrap();
        assert_eq!(staged_batch_count_in(&base), 3);
        // empty subdirectories with jsonl-ish names don't fool the scan
        std::fs::create_dir_all(base.join("weird.jsonl")).unwrap();
        assert_eq!(
            staged_batch_count_in(&base),
            3,
            "directories must not count"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
