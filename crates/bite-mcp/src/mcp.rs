//! MCP stdio server — a minimal, spec-stable implementation of the parts bite
//! needs (tools, resources, prompts) over newline-delimited JSON-RPC 2.0.
//! Hand-rolled on purpose: the stdio surface of MCP is tiny and stable, and
//! this keeps `bite` dependency-light and immune to SDK API churn.

use std::io::{BufRead, Write};

use bite_core::{json_schema, ops, registry, BiteError};
use serde_json::{json, Value};

use crate::helper::BridgeHandle;
use crate::idempotency;

const SERVER_NAME: &str = "bite-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

struct ServerState {
    /// Interior-mutable bridge holder: request threads fetch `Arc` clones
    /// and never hold a lock across a helper round-trip.
    handle: BridgeHandle,
    /// Outcomes recorded against client-supplied `idempotency_key`s (see
    /// `idempotency`); only the tools that declare the param consult it.
    idempotency: std::sync::Mutex<idempotency::Idempotency>,
}

pub fn serve() -> i32 {
    let state = std::sync::Arc::new(ServerState {
        handle: BridgeHandle::new(),
        idempotency: std::sync::Mutex::new(idempotency::Idempotency::default()),
    });
    let stdout = std::sync::Arc::new(std::io::stdout());
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut raw = Vec::new();
    // Unreadable stdin ends the loop (the `while let` arm) the same way the
    // old `lines()` iterator's read error did: the client is gone.
    while let Ok(outcome) = read_request_line(&mut input, &mut raw) {
        match outcome {
            LineRead::Eof => break,
            LineRead::Overlong => {
                // The line was never parsed, so the id is unknown — answer
                // with the null-id error form and keep serving the stream.
                write_line(&stdout, &overlong_line_response());
                continue;
            }
            LineRead::Line => {}
        }
        // Same contract the `lines()` iterator had: a line that is not
        // valid UTF-8 ends the loop (it arrived as a read error before).
        let Ok(line) = std::str::from_utf8(&raw) else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let resp = json!({
                    "jsonrpc": "2.0", "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                });
                write_line(&stdout, &resp);
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        // notifications (no id) get no response
        if request.get("id").is_none() {
            continue;
        }
        let method = request
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let params = request.get("params").cloned().unwrap_or(json!({}));
        // Dispatch each request on its own thread: a slow helper round-trip
        // (Apple Events can take the full per-call timeout) must not stall
        // unrelated traffic — pings, tools/list and other pipelined requests
        // are answered while a tool call is still in flight. JSON-RPC
        // responses correlate by id, so completion order is free. Threads
        // are detached: when stdin closes the client is gone and in-flight
        // work has no audience.
        let state = std::sync::Arc::clone(&state);
        let resp_stdout = std::sync::Arc::clone(&stdout);
        let resp_id = id.clone();
        let spawned = std::thread::Builder::new()
            .name("bite-mcp-request".to_owned())
            .spawn(move || {
                let result = handle_request(&state, &method, &params);
                let resp = match result {
                    Ok(v) => json!({ "jsonrpc": "2.0", "id": resp_id, "result": v }),
                    Err((code, message)) => {
                        json!({ "jsonrpc": "2.0", "id": resp_id, "error": { "code": code, "message": message } })
                    }
                };
                write_line(&resp_stdout, &resp);
            });
        if let Err(e) = spawned {
            let resp = json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32603, "message": format!("internal error: could not dispatch request: {e}")}
            });
            write_line(&stdout, &resp);
        }
    }
    0
}

fn write_line(out: &std::io::Stdout, v: &Value) {
    let mut lock = out.lock();
    let _ = writeln!(lock, "{v}");
    let _ = lock.flush();
}

/// Upper bound on a single newline-delimited JSON-RPC request line, the
/// trailing newline included. The MCP spec expects servers to cap message
/// size; 16 MiB is orders of magnitude above any legitimate bite request
/// (the largest arguments are mail search queries) while keeping a hostile
/// or buggy client from buffering a line of any size into server memory —
/// this is a long-lived process holding the LanceDB index and a live
/// helper. Same class of boundary guard as `MAX_MESSAGES_LIMIT`.
const MAX_REQUEST_LINE_BYTES: usize = 16 * 1024 * 1024;

/// The outcome of one bounded read from the request stream.
enum LineRead {
    /// A complete line (at or under the cap) is in the buffer, its
    /// trailing newline included.
    Line,
    /// The stream ended cleanly and nothing is buffered.
    Eof,
    /// The line exceeded `MAX_REQUEST_LINE_BYTES`. Its remainder has been
    /// drained and the buffer emptied, so the stream is positioned at the
    /// start of the next line and no cap-sized prefix is retained.
    Overlong,
}

/// Read one newline-terminated request line into `buf`, enforcing
/// `MAX_REQUEST_LINE_BYTES`. Memory stays capped no matter how large the
/// incoming line is: the line is assembled from the reader's own chunks,
/// and once the cap is crossed nothing more is buffered — the remainder is
/// discarded chunk by chunk until its newline arrives (or the stream
/// ends). A line of exactly `MAX_REQUEST_LINE_BYTES` bytes parses normally.
fn read_request_line(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> std::io::Result<LineRead> {
    buf.clear();
    let mut overlong = false;
    loop {
        // `fill_buf` borrows the reader; decide what to consume inside the
        // block, then release the borrow before `consume` takes it again.
        let (consume, done) = {
            let available = match reader.fill_buf() {
                Ok(chunk) => chunk,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if available.is_empty() {
                return Ok(if overlong {
                    // an unterminated flood ended at EOF — still overlong
                    LineRead::Overlong
                } else if buf.is_empty() {
                    LineRead::Eof
                } else {
                    // final unterminated line — delivered, like `lines()` does
                    LineRead::Line
                });
            }
            match available.iter().position(|&b| b == b'\n') {
                // Newline in this chunk: everything through it is the line.
                Some(pos) => {
                    let end = pos + 1;
                    if overlong || buf.len() + end > MAX_REQUEST_LINE_BYTES {
                        buf.clear();
                        (end, Some(LineRead::Overlong))
                    } else {
                        buf.extend_from_slice(&available[..end]);
                        (end, Some(LineRead::Line))
                    }
                }
                // No newline yet: buffer the chunk — or, once past the cap,
                // drop it and keep draining until the newline shows up.
                None => {
                    let n = available.len();
                    if overlong || buf.len() + n > MAX_REQUEST_LINE_BYTES {
                        overlong = true;
                        buf.clear();
                        (n, None)
                    } else {
                        buf.extend_from_slice(available);
                        (n, None)
                    }
                }
            }
        };
        reader.consume(consume);
        if let Some(outcome) = done {
            return Ok(outcome);
        }
    }
}

/// The JSON-RPC error answer for an over-cap request line. The line was
/// never parsed, so the id is unknown (the null-id form the spec requires)
/// and the rejection is -32600 invalid request rather than -32700 parse
/// error — the bytes are a message, just not one the server will accept.
fn overlong_line_response() -> Value {
    json!({
        "jsonrpc": "2.0", "id": Value::Null,
        "error": {
            "code": -32600,
            "message": format!(
                "invalid request: message exceeds the {}-byte maximum line length",
                MAX_REQUEST_LINE_BYTES
            )
        }
    })
}

type RpcError = (i32, String);

fn err(code: i32, message: impl Into<String>) -> RpcError {
    (code, message.into())
}

fn handle_request(state: &ServerState, method: &str, params: &Value) -> Result<Value, RpcError> {
    match method {
        "initialize" => {
            // echo the client's requested protocol version when it gave one
            let requested = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");
            Ok(json!({
                "protocolVersion": requested,
                "capabilities": {
                    "tools": { "listChanged": false },
                    "resources": { "subscribe": false, "listChanged": false },
                    "prompts": { "listChanged": false }
                },
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                "instructions": "bite gives native access to the user's Apple Calendar, Reminders, Mail, Notes, Contacts and Messages. Destructive calls (deletes, moves) return a preview until you pass confirm:true. Check calendar availability before scheduling. Permission errors include a `fix` — relay it to the user verbatim."
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => {
            let tools: Vec<Value> = bite_core::tools()
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": json_schema(t),
                        "annotations": {
                            "title": t.name.replace('_', " "),
                            "readOnlyHint": !t.destructive,
                            "destructiveHint": t.destructive,
                            "idempotentHint": false,
                            "openWorldHint": false,
                        }
                    })
                })
                .collect();
            Ok(json!({ "tools": tools }))
        }
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args =
                clamp_messages_limit(name, params.get("arguments").cloned().unwrap_or(json!({})));
            if name == "mail_messages_search" {
                // fast path: entity index (milliseconds, exact counts).
                // When the index has no Mail rows yet, degrade to a SHORT
                // live attempt — instant "indexing in progress" feedback
                // instead of hanging the caller for minutes.
                match crate::index::try_mail_search(&args) {
                    Some(Ok(value)) => {
                        return Ok(json!({
                            "content": [ { "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() } ],
                            "structuredContent": value,
                            "isError": false,
                        }));
                    }
                    Some(Err(e)) => return Err(err(-32000, e.render())),
                    None => {
                        let bridge = state
                            .handle
                            .get()
                            .map_err(|e| err(-32000, bite_core::BiteError::from(e).render()))?;
                        let short = std::time::Duration::from_secs(45);
                        match bite_core::ops::run_with_timeout(&bridge, name, args.clone(), short) {
                            Ok(value) => {
                                let mut value = value;
                                value["source"] = json!("live");
                                value["index_hint"] = json!("served via live Apple Events (slow) — the entity index is still being built; retry soon for instant results");
                                return Ok(json!({
                                    "content": [ { "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() } ],
                                    "structuredContent": value,
                                    "isError": false,
                                }));
                            }
                            Err(e) if e.code == "timeout" => {
                                return Ok(json!({
                                    "content": [ { "type": "text", "text": serde_json::to_string_pretty(&crate::index::not_ready_response("live Apple Events timed out — Mail is busy")).unwrap_or_default() } ],
                                    "structuredContent": crate::index::not_ready_response("live Apple Events timed out — Mail is busy"),
                                    "isError": false,
                                }));
                            }
                            Err(e) => {
                                return Err(err(-32000, bite_core::BiteError::from(e).render()))
                            }
                        }
                    }
                }
            }
            if crate::index::is_live_mail_tool(name) {
                // instant deferred response while Mail indexing runs — the
                // agent re-calls with force_live: true if it really wants to
                // pay the slow live price
                if let Some(deferred) = crate::index::gate_live_mail(name, &args) {
                    return Ok(json!({
                        "content": [ { "type": "text", "text": serde_json::to_string_pretty(&deferred).unwrap_or_default() } ],
                        "structuredContent": deferred,
                        "isError": false,
                    }));
                }
            }
            if crate::index::is_local_tool(name) {
                let value = crate::index::run_local_unwrapped(name, &args, Some(&state.handle))
                    .map_err(|e| err(-32000, e.render()))?;
                return Ok(json!({
                    "content": [ { "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() } ],
                    "structuredContent": value,
                    "isError": false,
                }));
            }
            let value =
                run_tool_idempotent(state, name, args).map_err(|e| err(-32000, e.render()))?;
            let is_error = is_tool_error(&value);
            Ok(json!({
                "content": [ { "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() } ],
                "structuredContent": value,
                "isError": is_error,
            }))
        }
        "resources/list" => Ok(json!({
            "resources": resources().iter().map(|r| {
                json!({
                    "uri": r["uri"], "name": r["name"], "description": r["description"],
                    "mimeType": "application/json"
                })
            }).collect::<Vec<_>>()
        })),
        "resources/read" => {
            let uri = params.get("uri").and_then(|u| u.as_str()).unwrap_or("");
            let list = resources();
            let tool = list
                .iter()
                .find(|r| r["uri"] == json!(uri))
                .and_then(|r| r["tool"].as_str())
                .ok_or_else(|| err(-32002, format!("unknown resource: {uri}")))?;
            let value = run_tool_retrying(state, tool, json!({}))
                .outcome
                .map_err(|e| err(-32000, e.render()))?;
            Ok(json!({
                "contents": [ {
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": serde_json::to_string_pretty(&value).unwrap_or_default(),
                } ]
            }))
        }
        "prompts/list" => Ok(json!({ "prompts": prompts_values() })),
        "prompts/get" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let text =
                prompt_text(name).ok_or_else(|| err(-32002, format!("unknown prompt: {name}")))?;
            Ok(json!({
                "description": text.1,
                "messages": [ { "role": "user", "content": { "type": "text", "text": text.0 } } ]
            }))
        }
        m if m.starts_with("notifications/") => Ok(json!({})),
        _ => Err(err(-32601, format!("method not found: {method}"))),
    }
}

/// `run_tool_retrying` wrapped with client-keyed idempotency for the tools
/// that declare `idempotency_key` (`mail_send` today): a repeated call with
/// the same key AND identical arguments replays the recorded outcome
/// instead of dispatching a second send, a key reused with DIFFERENT
/// arguments is refused with `idempotency_key_reused` (a replay would
/// silently discard the new arguments), a key whose attempt failed
/// ambiguously is refused outright, and a key whose call is still in flight
/// in another request thread is refused with `idempotency_in_flight` — the
/// claim and the lookup happen under one lock, so concurrent same-key
/// calls cannot both dispatch. The key is stripped from the forwarded
/// arguments, so the helper never sees it. Tools without the param — and
/// calls without a key — take exactly the `run_tool_retrying` path.
fn run_tool_idempotent(
    state: &ServerState,
    name: &str,
    mut args: Value,
) -> Result<Value, BiteError> {
    let keyed = registry::find(name)
        .map(|tool| tool.params.iter().any(|p| p.name == idempotency::KEY_PARAM))
        .unwrap_or(false);
    if !keyed {
        return run_tool_retrying(state, name, args).outcome;
    }
    let key = idempotency::take_key(&mut args).map_err(|message| {
        let mut e = bite_bridge::BridgeError::new("invalid_params", message);
        e.fix = Some(format!(
            "pass idempotency_key as a non-empty string (at most {} bytes), \
             or omit it",
            idempotency::MAX_KEY_LEN
        ));
        BiteError(e)
    })?;
    let Some(key) = key else {
        return run_tool_retrying(state, name, args).outcome;
    };
    // Fingerprint the stripped arguments this call would dispatch with: a
    // replay must return the recorded outcome for the SAME call, so a key
    // reused with different arguments is refused outright instead of
    // silently discarding them.
    let fingerprint = idempotency::fingerprint(&args);
    // Claim the key and read any prior state in ONE lock hold: requests are
    // served one thread per call, so a separate lookup and dispatch would
    // let two concurrent same-key calls both reach the helper.
    match state
        .idempotency
        .lock()
        .unwrap()
        .reserve(&key, &fingerprint)
    {
        idempotency::Reserved::Done(value) => return Ok(value),
        idempotency::Reserved::Reused => {
            let mut e = bite_bridge::BridgeError::new(
                "idempotency_key_reused",
                format!(
                    "this idempotency_key was already used for a {name} call with \
                     different arguments — a replay would silently discard the \
                     new arguments and return the earlier outcome, so this call \
                     was refused"
                ),
            );
            e.fix = Some(
                "reuse an idempotency_key only to retry the SAME call \
                 (identical arguments); use a fresh idempotency_key for new \
                 arguments"
                    .into(),
            );
            return Err(BiteError(e));
        }
        idempotency::Reserved::Unknown(code) => {
            let mut e = bite_bridge::BridgeError::new(
                "idempotency_unknown",
                format!(
                    "an earlier {name} with this idempotency_key failed ambiguously \
                     ({code}) — it may or may not have taken effect, so the repeat \
                     call was refused instead of risking a second dispatch"
                ),
            );
            e.fix = Some(
                "confirm the original outcome first (search before re-sending), then \
                 retry with a fresh idempotency_key if it truly did not go out"
                    .into(),
            );
            return Err(BiteError(e));
        }
        idempotency::Reserved::InFlight => {
            let mut e = bite_bridge::BridgeError::new(
                "idempotency_in_flight",
                format!(
                    "another {name} with this idempotency_key is still in flight in \
                     this server process — its outcome has not settled, so this call \
                     was refused instead of dispatching a second concurrent send"
                ),
            );
            e.fix = Some(
                "wait for the in-flight call to settle, then replay this exact call \
                 (a settled outcome replays verbatim), or use a fresh idempotency_key"
                    .into(),
            );
            return Err(BiteError(e));
        }
        idempotency::Reserved::Fresh => {}
    }
    let run = run_tool_retrying(state, name, args);
    match &run.outcome {
        Ok(value) => state
            .idempotency
            .lock()
            .unwrap()
            .settle(&key, idempotency::Settled::Done(value.clone())),
        // Pin only when a request write actually landed: the ambiguous
        // codes are also produced by a failed spawn/handshake (and by a
        // failed respawn after an unsent attempt), where provably nothing
        // was written — those release below so an immediate retry with
        // the same key stays possible.
        Err(e) if run.request_written && idempotency::ambiguous(e.code()) => state
            .idempotency
            .lock()
            .unwrap()
            .settle(&key, idempotency::Settled::Unknown(e.code().to_string())),
        // Clean failures provably did not send — release the claim so the
        // key stays free for an immediate retry.
        _ => state.idempotency.lock().unwrap().release(&key),
    }
    run.outcome
}

/// Upper bound for client-supplied `limit` values on the messages read
/// tools and `mail_messages_search`. Chats, history and live mail rows all
/// travel as full JSON payloads through the single bridge handle, so an
/// oversized ask pins the helper in a chat.db / Apple Events scan while
/// every other tool call queues behind it — same class of guard as the
/// index search cap (`index::MAX_SEARCH_LIMIT`).
pub(crate) const MAX_MESSAGES_LIMIT: u64 = 200;

/// Upper bound for client-supplied `max_scan` values on
/// `mail_messages_search`. The live Apple Events walk length is set by
/// `max_scan` (helper default 400) — `limit` only ends the walk early
/// once enough matches are found — so an oversized `max_scan` walks
/// millions of messages over the single bridge handle while every other
/// tool queues behind it. `max_scan <= 0` is worse: it reaches the
/// helper as a Swift `1...maxScan` range trap (crash + respawn loop).
pub(crate) const MAX_SCAN_LIMIT: u64 = 2000;

/// Clamp `limit` on `messages_chats_recent` / `messages_history` /
/// `mail_messages_search`, and `max_scan` on `mail_messages_search`, at
/// the MCP boundary: `limit` lands in `1..=MAX_MESSAGES_LIMIT`, `max_scan`
/// in `1..=MAX_SCAN_LIMIT`. Numbers are read the way the Swift helper
/// reads them (`J.optInt`: u64 directly, finite floats truncated toward
/// zero, booleans bridged to 0/1 the way NSNumber casts read them), so a
/// float- or boolean-valued `limit` or `max_scan` no longer slips the
/// clamp. Any other shape (absent, null, string, non-finite float) passes
/// through untouched — the helper keeps its existing default handling for
/// those.
///
/// For `mail_messages_search` this bounds the live bypasses: the index
/// fast path already caps rows via `index::search_limit`, but the
/// empty-index degrade (`run_with_timeout`) and a `force_live: true`
/// re-call forward the raw client values into a live Mail Apple Events
/// walk — and that walk is bounded by `max_scan`, not `limit`. Clamping
/// both here means every path shares one boundary on the rows returned
/// AND the messages examined.
fn clamp_messages_limit(name: &str, args: Value) -> Value {
    if !matches!(
        name,
        "messages_chats_recent" | "messages_history" | "mail_messages_search"
    ) {
        return args;
    }
    let mut clamped = args;
    if let Some(n) = clamped.get("limit").and_then(opt_int_u64) {
        clamped["limit"] = json!(n.clamp(1, MAX_MESSAGES_LIMIT));
    }
    if name == "mail_messages_search" {
        if let Some(n) = clamped.get("max_scan").and_then(opt_int_u64) {
            clamped["max_scan"] = json!(n.clamp(1, MAX_SCAN_LIMIT));
        }
    }
    clamped
}

/// Read a JSON value the way the helper's `J.optInt` does: u64 directly,
/// any other finite number truncated toward zero into u64 (the cast
/// saturates, so an enormous float still clamps instead of overflowing).
/// Booleans map to 0/1: JSONSerialization decodes `true`/`false` to
/// NSCFBoolean — an NSNumber subclass — so the helper's
/// `(p[key] as? NSNumber)?.intValue` reads them as 1/0, and a raw
/// `{"max_scan": false}` reached the Swift `1...maxScan` range trap as 0
/// before this mirror existed (review on #73). Non-numbers (booleans
/// excepted) and non-finite floats yield `None`.
fn opt_int_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Bool(b) => Some(*b as u64),
        _ => v.as_u64().or_else(|| {
            v.as_f64()
                .filter(|f| f.is_finite())
                .map(|f| f.max(0.0) as u64)
        }),
    }
}

/// The transport-level outcome of one tool run, plus whether any request
/// write actually landed on a helper transport during it. `request_written`
/// is `false` only when every attempt failed before a request line was
/// written to ANY helper — the spawn/handshake itself failed (no bridge was
/// ever produced), or the one attempt was provably unsent and the respawn's
/// handshake then failed too. The handshake failures reuse the ambiguous
/// `helper_exited`/`timeout` codes, so the idempotency layer consults this
/// flag instead of classifying by code alone: an ambiguous code WITH
/// `request_written == false` provably never sent and must not pin the key.
struct ToolRun {
    outcome: Result<Value, BiteError>,
    request_written: bool,
}

/// Run a tool through the bridge; if the helper exited, respawn once.
/// The `Arc<Bridge>` clone is held across the round-trip (the bridge client
/// multiplexes by request id); the handle's lock is touched only inside
/// `get`/`reset_if_same`, so concurrent requests never serialize behind
/// this call or its respawn.
fn run_tool_retrying(state: &ServerState, name: &str, args: Value) -> ToolRun {
    // A failed spawn/handshake here (helper exits before hello, or never
    // says it) reuses the ambiguous `helper_exited`/`timeout` codes even
    // though no request was ever written — hence `request_written: false`.
    let bridge = match state.handle.get() {
        Ok(bridge) => bridge,
        Err(e) => {
            return ToolRun {
                outcome: Err(BiteError(e)),
                request_written: false,
            }
        }
    };
    let mut request_written = false;
    let outcome = match ops::run(&bridge, name, args.clone()) {
        Ok(v) => {
            request_written = true; // a request landed and was answered
            Ok(v)
        }
        Err(mut e) if matches!(e.code.as_str(), "helper_exited" | "helper_exited_unsent") => {
            // Respawn either way so the next call gets a fresh process —
            // recycling only the failed generation: a sibling request thread
            // may already have respawned a helper that must not be killed
            // for an older generation's crash.
            // Plain helper_exited means the request WAS written and the
            // helper died before responding (it may have committed);
            // helper_exited_unsent means the write itself failed (the
            // request provably never reached the helper) — replaying is
            // safe for EVERY tool, mutating ones included.
            if e.code == "helper_exited" {
                request_written = true;
            }
            state.handle.reset_if_same(&bridge);
            if e.code == "helper_exited_unsent" || registry::replay_safe(name) {
                let fresh = match state.handle.get() {
                    Ok(fresh) => fresh,
                    // The respawn's own handshake failed: whatever codes it
                    // reuses, nothing further was written — keep the flag
                    // as tracked so an unsent first attempt stays unpinned.
                    Err(e2) => {
                        return ToolRun {
                            outcome: Err(BiteError(e2)),
                            request_written,
                        }
                    }
                };
                let replay = ops::run(&fresh, name, args);
                // Only a provably-unsent replay write leaves the flag
                // untouched; every other outcome landed on the transport.
                if !matches!(&replay, Err(re) if re.code == "helper_exited_unsent") {
                    request_written = true;
                }
                replay.map_err(BiteError)
            } else {
                e.message.push_str(
                    "; the helper was respawned but the call was not replayed — \
                     it may or may not have taken effect",
                );
                e.fix = Some(
                    "re-check state (search before re-sending, list before deleting) \
                     before re-issuing non-idempotent operations"
                        .to_string(),
                );
                Err(BiteError(e))
            }
        }
        // Alive but wedged (a stuck Apple Events call): recycle the helper so
        // the next call gets a fresh process instead of queueing behind the
        // stuck one while this mutex is held. The abandoned call is NOT
        // replayed — it may already have committed inside the helper, and a
        // blind retry could double-send mail or repeat a delete.
        Err(mut e) if e.code == "timeout" => {
            request_written = true; // written; the helper wedged on it
            state.handle.reset_if_same(&bridge);
            e.message.push_str(
                "; the wedged helper was recycled and the call was abandoned — \
                 it may or may not have taken effect",
            );
            e.fix = Some(
                "re-check state (search before re-sending, list before deleting) \
                 before re-issuing non-idempotent operations"
                    .to_string(),
            );
            Err(BiteError(e))
        }
        // Tool-level or protocol error: the request was written and the
        // helper answered it — the failure is clean, nothing sent.
        Err(e) => {
            request_written = true;
            Err(BiteError(e))
        }
    };
    ToolRun {
        outcome,
        request_written,
    }
}

/// The helper surfaces soft failures (preview-on-delete) as results; anything
/// with an `error`-shaped preview we flag as isError so agents pay attention.
fn is_tool_error(value: &Value) -> bool {
    value.get("error").is_some()
}

fn resources() -> Vec<Value> {
    vec![
        json!({"uri": "bite://calendars", "name": "Calendars", "description": "All calendars with ids", "tool": "calendar_list_calendars"}),
        json!({"uri": "bite://reminders/lists", "name": "Reminder lists", "description": "All reminder lists", "tool": "reminders_list_lists"}),
        json!({"uri": "bite://mail/mailboxes", "name": "Mailboxes", "description": "Mail mailboxes with unread counts", "tool": "mail_mailboxes_list"}),
        json!({"uri": "bite://notes/folders", "name": "Notes folders", "description": "Notes folders per account", "tool": "notes_folders"}),
        json!({"uri": "bite://contacts/groups", "name": "Contact groups", "description": "Contact groups with member counts", "tool": "contacts_groups"}),
    ]
}

const PROMPTS: &[(&str, &str, &str)] = &[
    (
        "plan-my-week",
        "Review the calendar and propose a weekly plan",
        "Look at my calendar events for the next 7 days with calendar_events_search, list my open reminders with reminders_search (due within 14 days), then propose a day-by-day plan that fits around my existing events. Ask before creating anything.",
    ),
    (
        "triage-inbox",
        "Triage unread mail into actions",
        "Search unread mail with mail_messages_search (mailbox INBOX, unread true). For each message give a one-line summary and a suggestion: reply, archive, flag, or delete. Do not act without my confirmation.",
    ),
    (
        "daily-brief",
        "Morning brief across all apps",
        "Give me a brief for today: today's events (calendar_events_search for today), reminders due today (reminders_search due_within_days 1), unread count and flagged mail (mail_messages_search), and any recent iMessages (messages_chats_recent). Keep it scannable.",
    ),
];

static PROMPTS_VALUES: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();

fn prompts_values() -> &'static Vec<Value> {
    PROMPTS_VALUES.get_or_init(|| {
        PROMPTS
            .iter()
            .map(|(name, description, _)| json!({"name": name, "description": description}))
            .collect()
    })
}

fn prompt_text(name: &str) -> Option<(String, &'static str)> {
    PROMPTS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, description, text)| (text.to_string(), *description))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `BufRead` handing out at most `chunk` bytes per `fill_buf` call —
    /// the incremental shape a piped stdin actually delivers, which
    /// `Cursor` (one big chunk) never exercises.
    struct Trickle {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl std::io::Read for Trickle {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let end = (self.pos + self.chunk).min(self.data.len());
            let n = (end - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl BufRead for Trickle {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            let end = (self.pos + self.chunk).min(self.data.len());
            Ok(&self.data[self.pos..end])
        }
        fn consume(&mut self, n: usize) {
            self.pos += n;
        }
    }

    #[test]
    fn bounded_reader_returns_complete_lines() {
        let mut cursor = std::io::Cursor::new(b"one\ntwo\n\nthree".to_vec());
        let mut buf = Vec::new();
        assert!(matches!(
            read_request_line(&mut cursor, &mut buf),
            Ok(LineRead::Line)
        ));
        assert_eq!(buf, b"one\n");
        assert!(matches!(
            read_request_line(&mut cursor, &mut buf),
            Ok(LineRead::Line)
        ));
        assert_eq!(buf, b"two\n");
        // a blank line is delivered too — serve()'s trim() check skips it
        assert!(matches!(
            read_request_line(&mut cursor, &mut buf),
            Ok(LineRead::Line)
        ));
        assert_eq!(buf, b"\n");
        // a final unterminated line still arrives, as `lines()` delivered it
        assert!(matches!(
            read_request_line(&mut cursor, &mut buf),
            Ok(LineRead::Line)
        ));
        assert_eq!(buf, b"three");
        assert!(matches!(
            read_request_line(&mut cursor, &mut buf),
            Ok(LineRead::Eof)
        ));
        // ... and stays at EOF on further reads
        assert!(matches!(
            read_request_line(&mut cursor, &mut buf),
            Ok(LineRead::Eof)
        ));
        assert!(buf.is_empty());
    }

    #[test]
    fn bounded_reader_accepts_a_line_at_exactly_the_cap() {
        // cap bytes including the trailing newline is the largest legal line
        let mut data = vec![b'a'; MAX_REQUEST_LINE_BYTES - 1];
        data.push(b'\n');
        data.extend_from_slice(b"{\"id\":1}\n");
        let mut cursor = std::io::Cursor::new(data);
        let mut buf = Vec::new();
        assert!(matches!(
            read_request_line(&mut cursor, &mut buf),
            Ok(LineRead::Line)
        ));
        assert_eq!(buf.len(), MAX_REQUEST_LINE_BYTES);
        // the stream continues cleanly at the next line
        assert!(matches!(
            read_request_line(&mut cursor, &mut buf),
            Ok(LineRead::Line)
        ));
        assert_eq!(buf, b"{\"id\":1}\n");
    }

    #[test]
    fn bounded_reader_rejects_one_byte_past_the_cap_and_keeps_serving() {
        let mut over = vec![b'a'; MAX_REQUEST_LINE_BYTES];
        over.push(b'\n');
        over.extend_from_slice(b"{\"id\":2}\n");
        let mut cursor = std::io::Cursor::new(over);
        let mut buf = Vec::new();
        assert!(matches!(
            read_request_line(&mut cursor, &mut buf),
            Ok(LineRead::Overlong)
        ));
        assert!(buf.is_empty(), "no cap-sized prefix is retained");
        // the over-long line was drained through its newline: the next
        // request parses normally — the server keeps serving
        assert!(matches!(
            read_request_line(&mut cursor, &mut buf),
            Ok(LineRead::Line)
        ));
        assert_eq!(buf, b"{\"id\":2}\n");
    }

    #[test]
    fn bounded_reader_caps_memory_when_chunks_trickle_in() {
        // cap + 1 bytes delivered 7 at a time, then a clean request — the
        // incremental path a real pipe takes
        let mut over = vec![b'a'; MAX_REQUEST_LINE_BYTES];
        over.push(b'\n');
        over.extend_from_slice(b"next\n");
        let mut trickle = Trickle {
            data: over,
            pos: 0,
            chunk: 7,
        };
        let mut buf = Vec::new();
        assert!(matches!(
            read_request_line(&mut trickle, &mut buf),
            Ok(LineRead::Overlong)
        ));
        assert!(buf.is_empty(), "the buffered prefix is dropped at the cap");
        assert!(matches!(
            read_request_line(&mut trickle, &mut buf),
            Ok(LineRead::Line)
        ));
        assert_eq!(buf, b"next\n");
    }

    #[test]
    fn bounded_reader_flags_an_unterminated_flood_at_eof() {
        // more than the cap with no newline ever, then the stream ends: the
        // drain holds no cap-sized buffer and still reports Overlong
        let flood = vec![b'x'; MAX_REQUEST_LINE_BYTES + 5];
        let mut trickle = Trickle {
            data: flood,
            pos: 0,
            chunk: 13,
        };
        let mut buf = Vec::new();
        assert!(matches!(
            read_request_line(&mut trickle, &mut buf),
            Ok(LineRead::Overlong)
        ));
        assert!(buf.is_empty(), "nothing is retained from the flood");
    }

    #[test]
    fn overlong_response_is_a_null_id_invalid_request_error() {
        let resp = overlong_line_response();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], Value::Null);
        assert_eq!(resp["error"]["code"], -32600);
        let message = resp["error"]["message"].as_str().expect("message");
        assert!(
            message.contains(&MAX_REQUEST_LINE_BYTES.to_string()),
            "the message names the byte cap: {message}"
        );
    }

    #[test]
    fn messages_limit_clamps_client_supplied_values() {
        for tool in [
            "messages_chats_recent",
            "messages_history",
            "mail_messages_search",
        ] {
            assert_eq!(
                clamp_messages_limit(tool, json!({ "limit": 50 })),
                json!({ "limit": 50 }),
                "{tool}: in-range limit untouched"
            );
            assert_eq!(
                clamp_messages_limit(tool, json!({ "limit": 200 })),
                json!({ "limit": 200 }),
                "{tool}: boundary limit untouched"
            );
            assert_eq!(
                clamp_messages_limit(tool, json!({ "limit": 10_000_000 })),
                json!({ "limit": 200 }),
                "{tool}: oversized limit clamped"
            );
            assert_eq!(
                clamp_messages_limit(tool, json!({ "limit": u64::MAX })),
                json!({ "limit": 200 }),
                "{tool}: u64::MAX clamped"
            );
            assert_eq!(
                clamp_messages_limit(tool, json!({ "limit": 0 })),
                json!({ "limit": 1 }),
                "{tool}: zero clamped to 1"
            );
            assert_eq!(
                clamp_messages_limit(tool, json!({ "limit": 1e7 })),
                json!({ "limit": 200 }),
                "{tool}: float-valued oversized limit clamped, not slipped"
            );
            assert_eq!(
                clamp_messages_limit(tool, json!({ "limit": 50.9 })),
                json!({ "limit": 50 }),
                "{tool}: float limit truncated toward zero like the helper's optInt"
            );
            assert_eq!(
                clamp_messages_limit(tool, json!({ "limit": -5 })),
                json!({ "limit": 1 }),
                "{tool}: negative number clamped to 1 (was passed through raw)"
            );
            assert_eq!(
                clamp_messages_limit(tool, json!({ "limit": false })),
                json!({ "limit": 1 }),
                "{tool}: boolean false bridges to 0 like NSNumber and clamps to 1"
            );
            assert_eq!(
                clamp_messages_limit(tool, json!({ "limit": true })),
                json!({ "limit": 1 }),
                "{tool}: boolean true bridges to 1 like NSNumber — in range"
            );
        }
        // the clamp fires before the index/live split, so a search carrying
        // other params keeps them verbatim alongside the clamped limit
        assert_eq!(
            clamp_messages_limit(
                "mail_messages_search",
                json!({ "query": "invoice", "limit": 10_000_000, "force_live": true })
            ),
            json!({ "query": "invoice", "limit": 200, "force_live": true })
        );
    }

    #[test]
    fn mail_search_max_scan_clamps_client_supplied_values() {
        // the live walk length is set by max_scan (Swift `1...maxScan`,
        // helper default 400), not by limit — clamp it at the same MCP
        // boundary: an oversized ask walks millions of messages over the
        // single bridge handle, and max_scan <= 0 is a Swift range trap
        // that crash-loops the helper
        assert_eq!(
            clamp_messages_limit("mail_messages_search", json!({ "max_scan": 400 })),
            json!({ "max_scan": 400 }),
            "default-scale max_scan untouched"
        );
        assert_eq!(
            clamp_messages_limit("mail_messages_search", json!({ "max_scan": 10_000_000 })),
            json!({ "max_scan": 2000 }),
            "oversized max_scan clamped — the walk itself is bounded"
        );
        assert_eq!(
            clamp_messages_limit("mail_messages_search", json!({ "max_scan": 0 })),
            json!({ "max_scan": 1 }),
            "zero clamped to 1 — no Swift 1...0 range trap"
        );
        assert_eq!(
            clamp_messages_limit("mail_messages_search", json!({ "max_scan": -5 })),
            json!({ "max_scan": 1 }),
            "negative clamped to 1"
        );
        assert_eq!(
            clamp_messages_limit("mail_messages_search", json!({ "max_scan": 1e7 })),
            json!({ "max_scan": 2000 }),
            "float max_scan clamped (optInt would truncate it to 10M)"
        );
        // JSON booleans decode via JSONSerialization to NSNumber subclasses
        // (NSCFBoolean), so the helper's optInt reads false as 0 — arming
        // the 1...maxScan trap — and true as 1; the clamp mirrors that
        // bridging so both land clamped instead of passing through raw
        assert_eq!(
            clamp_messages_limit(
                "mail_messages_search",
                json!({ "from": "x", "max_scan": false })
            ),
            json!({ "from": "x", "max_scan": 1 }),
            "boolean max_scan clamps the way the helper reads it — trap closed"
        );
        assert_eq!(
            clamp_messages_limit("mail_messages_search", json!({ "max_scan": true })),
            json!({ "max_scan": 1 }),
            "boolean true max_scan reads as 1, in range"
        );
        // absent / null / non-number shapes pass through (helper default 400)
        assert_eq!(
            clamp_messages_limit("mail_messages_search", json!({ "from": "x" })),
            json!({ "from": "x" })
        );
        assert_eq!(
            clamp_messages_limit("mail_messages_search", json!({ "max_scan": null })),
            json!({ "max_scan": null })
        );
        assert_eq!(
            clamp_messages_limit("mail_messages_search", json!({ "max_scan": "400" })),
            json!({ "max_scan": "400" })
        );
        // both bounds clamp together, other params verbatim — this is the
        // exact shape of the review's hazard call
        assert_eq!(
            clamp_messages_limit(
                "mail_messages_search",
                json!({ "from": "nobody@x", "max_scan": 10_000_000, "force_live": true })
            ),
            json!({ "from": "nobody@x", "max_scan": 2000, "force_live": true })
        );
        // max_scan clamping is mail-search-only: no other read tool takes it
        assert_eq!(
            clamp_messages_limit(
                "messages_history",
                json!({ "chat_id": "x", "max_scan": 10_000_000 })
            ),
            json!({ "chat_id": "x", "max_scan": 10_000_000 })
        );
    }

    #[test]
    fn messages_limit_leaves_other_shapes_and_tools_alone() {
        // absent / null / non-number limits pass through untouched — the
        // helper keeps its default handling for those
        assert_eq!(
            clamp_messages_limit("messages_history", json!({ "chat_id": "x" })),
            json!({ "chat_id": "x" })
        );
        assert_eq!(
            clamp_messages_limit("messages_chats_recent", json!({ "limit": null })),
            json!({ "limit": null })
        );
        assert_eq!(
            clamp_messages_limit("messages_chats_recent", json!({ "limit": "50" })),
            json!({ "limit": "50" })
        );
        // other tools never see a rewrite
        assert_eq!(
            clamp_messages_limit("calendar_events_search", json!({ "limit": 10_000_000 })),
            json!({ "limit": 10_000_000 })
        );
    }

    #[test]
    fn mail_search_limit_cap_matches_index_search_cap() {
        // `mail_messages_search` limits are clamped to MAX_MESSAGES_LIMIT
        // at this boundary, but the index fast path re-clamps via
        // `index::search_limit` to `index::MAX_SEARCH_LIMIT` — one boundary
        // expressed as two consts. If they drift, the index and live paths
        // disagree on the maximum rows again (index returns the lower cap
        // while the degrade/force_live paths return the higher one), which
        // is exactly the disagreement this clamp closed; moving one cap
        // must move the other, and this pin forces the decision.
        assert_eq!(
            MAX_MESSAGES_LIMIT,
            crate::index::MAX_SEARCH_LIMIT,
            "MAX_MESSAGES_LIMIT and index::MAX_SEARCH_LIMIT are one boundary — move them together"
        );
    }

    #[test]
    fn messages_limit_cap_matches_registry_docs() {
        for tool in [
            "messages_chats_recent",
            "messages_history",
            "mail_messages_search",
        ] {
            let spec = bite_core::registry::find(tool).expect("tool registered");
            let limit_param = spec
                .params
                .iter()
                .find(|p| p.name == "limit")
                .expect("limit param");
            assert!(
                limit_param
                    .desc
                    .contains(&format!("capped at {MAX_MESSAGES_LIMIT}")),
                "{tool} registry doc must mirror the boundary cap: {}",
                limit_param.desc
            );
            if tool == "mail_messages_search" {
                // the walk bound gets the same doc-mirror treatment: the
                // registry must state the max_scan ceiling clients get
                let scan_param = spec
                    .params
                    .iter()
                    .find(|p| p.name == "max_scan")
                    .expect("max_scan param");
                assert!(
                    scan_param
                        .desc
                        .contains(&format!("capped at {MAX_SCAN_LIMIT}")),
                    "mail_messages_search registry doc must mirror the scan cap: {}",
                    scan_param.desc
                );
            }
        }
    }
}
