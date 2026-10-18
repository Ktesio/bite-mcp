//! End-to-end MCP stdio smoke test: launches `bite mcp` with the scripted
//! fake helper and exercises the protocol surface.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value};

/// The fake helper is a bin of bite-bridge; locate it in the shared target dir.
fn fake_helper_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test exe");
    let debug_dir = exe.ancestors().nth(2).expect("target dir").to_path_buf();
    let candidate = debug_dir.join("bite-fake-helper");
    assert!(
        candidate.exists(),
        "bite-fake-helper not built at {} — run `cargo test --workspace`",
        candidate.display()
    );
    candidate
}

struct McpProc {
    child: Child,
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: i64,
    /// Scenario file for the spawned server; lives (and is deleted) with the
    /// proc — held only for its Drop.
    _scenario: Option<tempfile::NamedTempFile>,
    /// Same file as `_scenario`, addressable for mid-test rewrites.
    scenario_path: std::path::PathBuf,
}

impl McpProc {
    fn start() -> Self {
        Self::start_scenario(json!({ "echo": true }), &[])
    }

    /// Start with an explicit helper scenario and extra env for the server
    /// process (e.g. `BITE_TIMEOUT_SECS` to bound per-call budgets).
    fn start_scenario(scenario: Value, extra_env: &[(&str, &str)]) -> Self {
        let mut scenario_file = tempfile::Builder::new()
            .prefix("bite-mcp-smoke-")
            .suffix(".json")
            .tempfile()
            .expect("scenario temp file");
        write!(scenario_file, "{}", scenario).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bite"));
        cmd.args(["mcp"])
            .env("BITE_HELPER_BIN", fake_helper_path())
            .env("BITE_FAKE_SCENARIO", scenario_file.path())
            .envs(extra_env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().expect("spawn bite mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        Self {
            scenario_path: scenario_file.path().to_path_buf(),
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 0,
            _scenario: Some(scenario_file),
        }
    }

    /// Replace the helper scenario on disk — only a helper (re)spawned after
    /// this point reads the new content; a live one keeps its old script.
    fn rewrite_scenario(&self, scenario: Value) {
        std::fs::write(&self.scenario_path, scenario.to_string()).expect("rewrite scenario");
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let line =
            json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params });
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
        let mut buf = String::new();
        self.reader.read_line(&mut buf).expect("response line");
        let v: Value = serde_json::from_str(buf.trim()).expect("valid json response");
        assert_eq!(v["id"], json!(self.next_id));
        v["result"].clone()
    }

    /// One raw request returning the full response object (errors included).
    fn request_raw(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let line =
            json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params });
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
        let mut buf = String::new();
        self.reader.read_line(&mut buf).expect("response line");
        serde_json::from_str(buf.trim()).expect("valid json response")
    }
}

impl Drop for McpProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_end_to_end() {
    let mut mcp = McpProc::start();

    // initialize
    let init = mcp.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "smoke", "version": "0" }
        }),
    );
    assert_eq!(init["protocolVersion"], json!("2025-06-18"));
    assert_eq!(init["serverInfo"]["name"], json!("bite-mcp"));
    assert!(init["capabilities"]["tools"].is_object());

    // tools/list: full surface
    let tools = mcp.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.len() >= 38,
        "expected >=38 tools, got {}",
        names.len()
    );
    for required in [
        "calendar_event_create",
        "calendar_availability",
        "reminders_create",
        "mail_send",
        "mail_attachment_save",
        "notes_create",
        "contacts_search",
        "messages_history",
    ] {
        assert!(names.contains(&required), "missing tool {required}");
    }
    // schemas present
    let create = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("calendar_event_create"))
        .unwrap();
    assert_eq!(
        create["inputSchema"]["properties"]["title"]["type"],
        json!("string")
    );

    // tools/call routes through to the helper
    let call = mcp.request(
        "tools/call",
        json!({
            "name": "calendar_list_calendars",
            "arguments": {}
        }),
    );
    assert_eq!(call["isError"], json!(false));
    let text = call["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["ok"], json!(true));

    // resources
    let resources = mcp.request("resources/list", json!({}));
    assert!(resources["resources"].as_array().unwrap().len() >= 5);
    let read = mcp.request("resources/read", json!({ "uri": "bite://calendars" }));
    assert_eq!(read["contents"][0]["uri"], json!("bite://calendars"));

    // prompts
    let prompts = mcp.request("prompts/list", json!({}));
    let pnames: Vec<&str> = prompts["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(pnames.contains(&"plan-my-week"));
    let got = mcp.request("prompts/get", json!({ "name": "daily-brief" }));
    assert!(got["messages"][0]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("brief"));

    // unknown method → -32601
    self::assert_unknown_method(&mut mcp);
}

fn assert_unknown_method(mcp: &mut McpProc) {
    mcp.next_id += 1;
    let line =
        json!({ "jsonrpc": "2.0", "id": mcp.next_id, "method": "no/such/method", "params": {} });
    writeln!(mcp.stdin, "{line}").unwrap();
    mcp.stdin.flush().unwrap();
    let mut buf = String::new();
    mcp.reader.read_line(&mut buf).unwrap();
    let v: Value = serde_json::from_str(buf.trim()).unwrap();
    assert_eq!(v["error"]["code"], json!(-32601));
}

/// A wedged helper (alive but stuck past the per-call budget) must surface a
/// timeout error, be recycled, and NOT have its call blindly replayed; the
/// next call must succeed through a fresh helper instead of queueing behind
/// the stuck one.
#[test]
fn wedged_helper_times_out_and_recycles() {
    let mut mcp = McpProc::start_scenario(
        json!({ "echo": true, "sleep_ms_per_call": 30_000 }),
        &[("BITE_TIMEOUT_SECS", "1")],
    );

    let init = mcp.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "smoke", "version": "0" }
        }),
    );
    assert_eq!(init["serverInfo"]["name"], json!("bite-mcp"));

    // sleeps past the 1s budget → timeout error that says the helper was
    // recycled and the call abandoned (with a fix hint), not a JSON-RPC hang
    let resp = mcp.request_raw(
        "tools/call",
        json!({ "name": "calendar_list_calendars", "arguments": {} }),
    );
    assert_eq!(resp["error"]["code"], json!(-32000));
    let msg = resp["error"]["message"].as_str().expect("error message");
    assert!(
        msg.contains("recycled"),
        "message should mention the recycle: {msg}"
    );
    assert!(
        msg.contains("abandoned"),
        "message should mention the abandoned call: {msg}"
    );
    assert!(
        msg.contains("fix:"),
        "message should carry the fix hint: {msg}"
    );

    // recovery: only a respawned helper reads the rewritten scenario — if the
    // wedged process were still behind the handle, this call would time out
    mcp.rewrite_scenario(json!({ "echo": true }));
    let call = mcp.request(
        "tools/call",
        json!({ "name": "calendar_list_calendars", "arguments": {} }),
    );
    assert_eq!(call["isError"], json!(false));
    let text = call["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["ok"], json!(true));
}
