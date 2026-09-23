//! Entity-index integration for the control plane: the LanceDB store, the
//! staged-batch ingester, the local (non-helper) index tools, and the
//! index-backed fast path for Mail search.

use std::sync::Mutex;
use std::time::SystemTime;

use bite_bridge::BridgeError;
use bite_core::error::cli_error;
use bite_core::BiteError;
use serde_json::{json, Value};

use crate::helper::BridgeHandle;

static INDEX: Mutex<Option<bite_index::EntityIndex>> = Mutex::new(None);
/// Process-level guard: auto-refresh spawns at most one crawl per run.
static AUTO_REFRESHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn dir() -> std::path::PathBuf {
    bite_core::config::data_dir().join("index.lance")
}

fn staging() -> std::path::PathBuf {
    bite_core::config::data_dir().join("batches")
}

/// Open (or reuse) the entity index and run `f`.
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
    f(guard.as_ref().unwrap())
}

/// Ingest any staged JSONL batches. Cheap when nothing is pending.
pub fn ingest_pending() -> Result<Value, BiteError> {
    let staging = staging();
    with_index(|index| {
        let (batches, rows) = bite_index::ingest_staged(index, &staging)
            .map_err(|e| BridgeError::new("index_ingest_failed", e.to_string()))?;
        crate::jobs::set_pending_batches(0);
        Ok(json!({ "batches_ingested": batches, "rows": rows }))
    })
}

fn pending_batches() -> usize {
    std::fs::read_dir(staging())
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

pub fn status() -> Result<Value, BiteError> {
    with_index(|index| {
        let stats = index
            .stats()
            .map_err(|e| BridgeError::new("index_error", e.to_string()))?;
        Ok(json!({
            "total": stats.total,
            "per_app": stats.per_app.into_iter().map(|(a, n)| json!({"app": a, "rows": n})).collect::<Vec<_>>(),
            "newest_updated_ms": stats.newest_updated_ms,
            "fts_indexed": stats.fts_indexed,
            "pending_batches": pending_batches(),
            "crawl": {
                "state": crawl_state(),
                "alive": crawl_pid().map(pid_alive).unwrap_or(false),
            },
            "jobs": crate::jobs::snapshot(),
        }))
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

fn crawl_running() -> bool {
    crawl_pid().map(pid_alive).unwrap_or(false)
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

/// Mail search served from the entity index. Returns None when the index has
/// no Mail rows yet (caller falls back to the live Apple Events path).
pub fn try_mail_search(params: &Value) -> Option<Result<Value, BiteError>> {
    let mail_rows = mail_rows_present()?;
    if mail_rows == 0 {
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

/// Mail search fast path — returns Some(result) when served from the index.
pub fn try_search_local(params: &Value) -> Option<Result<Value, BiteError>> {
    auto_refresh_if_stale();
    try_mail_search(params)
}

/// Kick the crawler when the index is stale and no crawl is running. Fires
/// at most once per control-plane process.
fn auto_refresh_if_stale() {
    if AUTO_REFRESHED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let state = crawl_state();
    let is_running =
        state.get("state").and_then(|v| v.as_str()) == Some("running") && crawl_running();
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
            SystemTime::now()
                .duration_since(t.into())
                .map(|d| d.as_secs())
                .unwrap_or(0)
        })
        .unwrap_or(u64::MAX);
    if stale / 3600 >= hours {
        let _ = spawn_crawl(&json!({}));
    }
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
    match crawl_pid() {
        Some(pid) if pid_alive(pid) => {
            let ok = std::process::Command::new("kill")
                .args([&pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            Ok(json!({ "cancelled": ok, "pid": pid }))
        }
        Some(_) => Ok(json!({ "cancelled": false, "note": "no running crawler" })),
        None => Ok(json!({ "cancelled": false, "note": "no crawler pidfile" })),
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
    let flagged = params.get("flagged").and_then(|v| v.as_bool());
    let junk = params.get("junk").and_then(|v| v.as_bool());
    let older = params.get("older_than_days").and_then(|v| v.as_i64());

    let selection_label = {
        let mut parts: Vec<String> = Vec::new();
        if let Some(true) = unread {
            parts.push("unread".into());
        }
        if let Some(true) = flagged {
            parts.push("flagged".into());
        }
        if let Some(true) = junk {
            parts.push("junk".into());
        }
        if let Some(d) = older {
            parts.push(format!("older than {d} days"));
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
    if unread == Some(true) {
        cmd.arg("--unread");
    }
    if older.is_some() {
        if let Some(d) = older {
            cmd.args(["--older-than-days", &d.to_string()]);
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
