//! Entity-index integration for the control plane: the LanceDB store, the
//! staged-batch ingester, and the local (non-helper) index tools.

use std::sync::Mutex;

use bite_bridge::BridgeError;
use bite_core::error::cli_error;
use bite_core::BiteError;
use serde_json::{json, Value};

use crate::helper::BridgeHandle;

static INDEX: Mutex<Option<bite_index::EntityIndex>> = Mutex::new(None);

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

fn rebuild(params: &Value, handle: &mut BridgeHandle) -> Result<Value, BiteError> {
    // The crawl runs in a DETACHED bite-crawl process (survives CLI/MCP
    // exit). It writes JSONL batches into staging; this process ingests
    // them. Warm the helper first so the TCC identity is established.
    let _ = handle.get()?;
    let bin = crawl_binary()?;

    if let Some(pid) = crawl_pid() {
        if pid_alive(pid) {
            return Ok(json!({
                "already_running": true,
                "pid": pid,
                "state": crawl_state(),
            }));
        }
    }

    let mut cmd = std::process::Command::new(&bin);
    if let Some(days) = params.get("window_days").and_then(|v| v.as_i64()) {
        cmd.args(["--window-days", &days.to_string()]);
    }
    if let Some(mailbox) = params.get("mailbox").and_then(|v| v.as_str()) {
        cmd.args(["--mailbox", mailbox]);
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

fn cancel(handle: &mut BridgeHandle) -> Result<Value, BiteError> {
    // warm/verify helper (keeps doctor parity); cancel is pid-based
    let _ = handle.get()?;
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

#[allow(dead_code)]
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

/// Tools served locally by the control plane (no helper round-trip).
pub const LOCAL_TOOLS: &[&str] = &["index_rebuild", "index_status", "index_crawl_cancel"];

pub fn is_local_tool(name: &str) -> bool {
    LOCAL_TOOLS.contains(&name)
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
                base["crawl"] = c;
            }
            Some(Ok(base))
        }
        "index_rebuild" => match handle {
            Some(h) => Some(rebuild(params, h)),
            None => Some(Err(cli_error("helper handle required"))),
        },
        "index_crawl_cancel" => match handle {
            Some(h) => Some(cancel(h)),
            None => Some(Err(cli_error("helper handle required"))),
        },
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
