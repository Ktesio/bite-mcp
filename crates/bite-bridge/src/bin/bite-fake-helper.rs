//! A scripted fake helper used by tests and MCP smoke tests.
//!
//! Usage: `bite-fake-helper <scenario.json>`
//! Scenario shape:
//! ```json
//! {
//!   "responses": { "sys.ping": [{"pong": true}], "calendar.list_calendars": [ ... ] },
//!   "echo": true,             // respond {"ok":true,"method":<method>} to anything not in responses
//!   "sleep_ms_per_call": 0    // sleep before every response (timeout tests)
//! }
//! ```
//! Each method consumes entries from its queue in order; the last entry repeats.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::process::exit;

use serde_json::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scenario: Value = if args.len() > 1 {
        let text = std::fs::read_to_string(&args[1]).expect("scenario file");
        serde_json::from_str(&text).expect("scenario json")
    } else {
        serde_json::json!({ "echo": true })
    };
    let echo = scenario
        .get("echo")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let sleep_ms = scenario
        .get("sleep_ms_per_call")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let mut responses: HashMap<String, Vec<Value>> = HashMap::new();
    if let Some(map) = scenario.get("responses").and_then(|v| v.as_object()) {
        for (k, v) in map {
            if let Some(arr) = v.as_array() {
                responses.insert(k.clone(), arr.clone());
            }
        }
    }

    let hello = serde_json::json!({
        "method": "hello",
        "params": { "protocol": 1, "version": "fake-0.1.0", "capabilities": ["calendar","reminders","contacts","mail","notes","messages"] }
    });
    println!("{hello}");
    let _ = std::io::stdout().flush();
    if scenario
        .get("exit_after_hello")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        exit(0);
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { exit(0) };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let method = v
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        if sleep_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
        }
        let out = match responses.get_mut(&method) {
            Some(queue) if !queue.is_empty() => {
                let item = if queue.len() == 1 {
                    queue[0].clone()
                } else {
                    queue.remove(0)
                };
                if let Some(err) = item.get("error") {
                    serde_json::json!({ "id": id, "error": err })
                } else {
                    serde_json::json!({ "id": id, "result": item })
                }
            }
            _ if echo => {
                serde_json::json!({ "id": id, "result": { "ok": true, "method": method } })
            }
            _ => {
                serde_json::json!({ "id": id, "error": { "code": "unknown_method", "message": format!("no scripted response for {method}") } })
            }
        };
        println!("{out}");
        let _ = std::io::stdout().flush();
    }
}
