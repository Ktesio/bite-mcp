//! MCP stdio server — a minimal, spec-stable implementation of the parts bite
//! needs (tools, resources, prompts) over newline-delimited JSON-RPC 2.0.
//! Hand-rolled on purpose: the stdio surface of MCP is tiny and stable, and
//! this keeps `bite` dependency-light and immune to SDK API churn.

use std::io::{BufRead, Write};
use std::sync::Mutex;

use bite_core::{json_schema, ops, BiteError};
use serde_json::{json, Value};

use crate::helper::BridgeHandle;

const SERVER_NAME: &str = "bite-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

struct ServerState {
    handle: Mutex<BridgeHandle>,
}

pub fn serve() -> i32 {
    let state = ServerState {
        handle: Mutex::new(BridgeHandle::new()),
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
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
        let result = handle_request(&state, &method, &params);
        let resp = match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message} })
            }
        };
        write_line(&stdout, &resp);
    }
    0
}

fn write_line(out: &std::io::Stdout, v: &Value) {
    let mut lock = out.lock();
    let _ = writeln!(lock, "{v}");
    let _ = lock.flush();
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
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            if name == "mail_messages_search" {
                // fast path: entity index (falls back to live Apple Events
                // automatically when the index has no Mail rows yet)
                match crate::index::try_mail_search(&args) {
                    Some(Ok(value)) => {
                        return Ok(json!({
                            "content": [ { "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() } ],
                            "structuredContent": value,
                            "isError": false,
                        }));
                    }
                    Some(Err(e)) => return Err(err(-32000, e.render())),
                    None => {} // fall through to live path
                }
            }
            if crate::index::is_local_tool(name) {
                let mut handle = state.handle.lock().unwrap();
                let value = crate::index::run_local_unwrapped(name, &args, Some(&mut handle))
                    .map_err(|e| err(-32000, e.render()))?;
                return Ok(json!({
                    "content": [ { "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() } ],
                    "structuredContent": value,
                    "isError": false,
                }));
            }
            let value =
                run_tool_retrying(state, name, args).map_err(|e| err(-32000, e.render()))?;
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
            let value =
                run_tool_retrying(state, tool, json!({})).map_err(|e| err(-32000, e.render()))?;
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

/// Run a tool through the bridge; if the helper exited, respawn once.
fn run_tool_retrying(state: &ServerState, name: &str, args: Value) -> Result<Value, BiteError> {
    let mut handle = state.handle.lock().unwrap();
    match ops::run(handle.get()?, name, args.clone()) {
        Ok(v) => Ok(v),
        Err(e) if e.code == "helper_exited" => {
            handle.reset();
            ops::run(handle.get()?, name, args).map_err(BiteError)
        }
        Err(e) => Err(BiteError(e)),
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
