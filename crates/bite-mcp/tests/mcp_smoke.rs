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

/// When the helper DIES mid-call (process exit, not a wedge), read-only tools
/// are transparently replayed against the respawned helper — the caller never
/// notices. The rewrite discriminator proves the replay really went to a
/// fresh process: the first helper is spawned lazily by a warm-up call (so
/// it holds the spawn-time script whose only scripted response for the
/// method is to exit), while a respawn re-reads the rewritten file.
#[test]
fn helper_death_replays_read_tools_transparently() {
    let mut mcp = McpProc::start_scenario(
        json!({ "echo": true, "responses": { "calendar.list_calendars": [{ "exit": true }] } }),
        &[],
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

    // Warm-up: spawns the first helper NOW, while the scenario file still
    // holds the exit script (helpers read the file once, at spawn).
    let warmup = mcp.request(
        "tools/call",
        json!({ "name": "notes_folders", "arguments": {} }),
    );
    assert_eq!(warmup["isError"], json!(false));

    // The respawned helper reads this script; the live one keeps its exit
    // entry queued and dies on the call.
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

/// A mutating tool (mail_send) whose helper died mid-call must NOT be
/// replayed — the first attempt may already have committed, and a blind
/// retry could double-send. The guard must still respawn the helper. The
/// rewrite discriminator: if a replay happened, it would hit the respawned
/// helper (which reads the rewritten script and answers with a marker) and
/// the call would SUCCEED; with the guard the caller gets an explicit
/// not-replayed error instead.
#[test]
fn helper_death_never_replays_mutating_tools() {
    let mut mcp = McpProc::start_scenario(
        json!({ "echo": true, "responses": { "mail.send": [{ "exit": true }] } }),
        &[],
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

    // Warm-up read: spawns the first helper on the exit script.
    let warmup = mcp.request(
        "tools/call",
        json!({ "name": "calendar_list_calendars", "arguments": {} }),
    );
    assert_eq!(warmup["isError"], json!(false));

    // Any replay would consume THIS response and return success — proving
    // the discriminator catches an unguarded retry.
    mcp.rewrite_scenario(json!({
        "echo": true,
        "responses": { "mail.send": [{ "sent": true, "would_be": "a second dispatch" }] }
    }));

    let resp = mcp.request_raw(
        "tools/call",
        json!({
            "name": "mail_send",
            "arguments": { "to": ["a@b.c"], "subject": "hi", "body": "x" }
        }),
    );
    assert_eq!(resp["error"]["code"], json!(-32000));
    let msg = resp["error"]["message"].as_str().expect("error message");
    assert!(
        msg.contains("not replayed"),
        "message should say the call was not replayed: {msg}"
    );
    assert!(
        msg.contains("may or may not have taken effect"),
        "message should disclose the uncertain outcome: {msg}"
    );
    assert!(
        msg.contains("fix:"),
        "message should carry the fix hint: {msg}"
    );

    // The helper WAS respawned: a follow-up read succeeds through the fresh
    // process (which serves the rewritten script).
    let call = mcp.request(
        "tools/call",
        json!({ "name": "calendar_list_calendars", "arguments": {} }),
    );
    assert_eq!(call["isError"], json!(false));
}

/// A mutating tool (mail_send) whose helper died BEFORE the request was
/// delivered IS safely replayed against the respawned helper: an unsent
/// death (transport already torn down when the call was made) proves
/// nothing ran the first time, so a retry cannot double-send. The rewrite
/// discriminator proves the replay really reached the fresh process: the
/// respawned helper reads the rewritten script and answers with a marker
/// only a replay would return.
#[test]
fn unsent_death_replays_mutating_tools_safely() {
    let mut mcp = McpProc::start_scenario(json!({ "echo": true, "exit_after_ms": 1000 }), &[]);

    let init = mcp.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "smoke", "version": "0" }
        }),
    );
    assert_eq!(init["serverInfo"]["name"], json!("bite-mcp"));

    // Warm-up read: spawns helper H1 now; it echoes and then exits on its
    // own ~200ms later (quiet death, nothing pending).
    let warmup = mcp.request(
        "tools/call",
        json!({ "name": "notes_folders", "arguments": {} }),
    );
    assert_eq!(warmup["isError"], json!(false));

    // Wait past H1's death so the mail_send below finds the transport
    // already torn down (unsent death) rather than racing a mid-call
    // death (ambiguous, correctly NOT replayed). The 2.5s margin covers
    // slow runners where the warm-up itself eats into the 1s deadline.
    std::thread::sleep(std::time::Duration::from_millis(2500));

    // Only the respawned helper reads this script; the replay must consume
    // its marker response and succeed.
    mcp.rewrite_scenario(json!({
        "echo": true,
        "responses": { "mail.send": [{ "sent": true, "delivered_by": "safe-replay" }] }
    }));

    let call = mcp.request(
        "tools/call",
        json!({
            "name": "mail_send",
            "arguments": { "to": ["a@b.c"], "subject": "hi", "body": "x" }
        }),
    );
    assert_eq!(call["isError"], json!(false));
    let text = call["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        parsed["delivered_by"],
        json!("safe-replay"),
        "the mutating call must be replayed against the respawned helper"
    );
}
