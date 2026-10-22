//! Golden-conversation tests against the scripted fake helper.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use bite_bridge::{Bridge, BridgeError};
use serde_json::json;

fn fake_helper() -> &'static str {
    env!("CARGO_BIN_EXE_bite-fake-helper")
}

/// Scenario file in a self-cleaning temp path (removed on drop — panics
/// can't leak `bite-fake-scenario-*` files in $TMPDIR).
fn scenario_file(scenario: &serde_json::Value) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .prefix("bite-fake-scenario-")
        .suffix(".json")
        .tempfile()
        .expect("create temp file");
    write!(f, "{}", serde_json::to_string(scenario).unwrap()).unwrap();
    f
}

#[test]
fn handshake_and_echo() {
    let bridge = Bridge::spawn(fake_helper()).unwrap();
    assert_eq!(bridge.hello().unwrap().protocol, 1);
    assert!(bridge.capabilities().contains(&"calendar".to_string()));
    let res = bridge.call("sys.ping", &json!({})).unwrap();
    assert_eq!(res["ok"], json!(true));
    assert_eq!(res["method"], json!("sys.ping"));
}

#[test]
fn sequential_requests_correlate() {
    let bridge = Bridge::spawn(fake_helper()).unwrap();
    for i in 0..5 {
        let res = bridge
            .call("calendar.list_calendars", &json!({ "n": i }))
            .unwrap();
        assert_eq!(res["ok"], json!(true));
    }
}

#[test]
fn scripted_error_is_surfaced() {
    let path = scenario_file(&json!({
        "responses": {
            "mail.accounts": [
                { "error": { "code": "permission_denied", "message": "denied", "app": "Mail", "fix": "open settings" } }
            ]
        }
    }));
    let bridge = Bridge::spawn_args(fake_helper(), &[path.path().display().to_string()]).unwrap();
    let err: BridgeError = bridge.call("mail.accounts", &json!({})).unwrap_err();
    assert!(err.is_permission());
    assert_eq!(err.app.as_deref(), Some("Mail"));
}

#[test]
fn timeout_is_reported_and_bridge_survives() {
    let path = scenario_file(&json!({ "echo": true, "sleep_ms_per_call": 1500 }));
    let bridge = Bridge::spawn_args(fake_helper(), &[path.path().display().to_string()]).unwrap();
    let err = bridge
        .call_timeout("sys.ping", &json!({}), Duration::from_millis(200))
        .unwrap_err();
    assert_eq!(err.code, "timeout");
    // the slow response is discarded; next fast request still works
    let res = bridge
        .call_timeout("sys.ping", &json!({}), Duration::from_secs(5))
        .unwrap();
    assert_eq!(res["ok"], json!(true));
}

#[test]
fn helper_exit_fails_pending_requests() {
    // helper prints hello then exits: either spawn reports the exit, or the
    // first call fails with helper_exited — never a silent success.
    let path = scenario_file(&json!({ "exit_after_hello": true }));
    let err = match Bridge::spawn_args(fake_helper(), &[path.path().display().to_string()]) {
        Err(e) => e,
        Ok(bridge) => bridge
            .call("sys.ping", &json!({}))
            .expect_err("call should fail"),
    };
    assert!(
        matches!(
            err.code.as_str(),
            "helper_exited" | "timeout" | "protocol_error"
        ),
        "unexpected code: {}",
        err.code
    );
}

#[test]
fn undelivered_call_reports_helper_exited_unsent() {
    // helper dies quietly ~150ms after hello; the next call finds the
    // transport already torn down (stdin cleared, nothing written) and must
    // report the request as never-delivered — a code distinct from a
    // mid-call death, where the outcome is genuinely unknown and a replay
    // of a mutating tool would be unsafe.
    let path = scenario_file(&json!({ "exit_after_ms": 400 }));
    let bridge = Bridge::spawn_args(fake_helper(), &[path.path().display().to_string()]).unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    let err = bridge.call("sys.ping", &json!({})).unwrap_err();
    assert_eq!(err.code, "helper_exited_unsent");
    // and the failure is sticky: the transport stays dead, never a
    // silent success on a later call
    let err2 = bridge.call("sys.ping", &json!({})).unwrap_err();
    assert_eq!(err2.code, "helper_exited_unsent");
}

#[test]
fn spawn_failure_is_reported() {
    let err = Bridge::spawn("/nonexistent/bite-helper-xyz")
        .err()
        .expect("should fail");
    assert_eq!(err.code, "helper_spawn_failed");
    let _ = Command::new("true")
        .stdout(Stdio::null())
        .stdin(Stdio::piped())
        .output();
    let _ = std::io::stdout().flush();
}

/// The bridge client multiplexes by request id: concurrent round-trips over
/// ONE transport interleave instead of serializing. The fake helper answers
/// each request on its own thread after a fixed delay, so four concurrent
/// calls must finish in roughly one delay — a serialized client (or helper)
/// would take four.
#[test]
fn concurrent_calls_multiplex_one_bridge() {
    let scenario = scenario_file(&json!({
        "echo": true,
        "sleep_ms_per_call": 300,
        "handler_threads": 4,
    }));
    let bridge = Bridge::spawn_args(
        fake_helper(),
        &[scenario.path().to_string_lossy().to_string()],
    )
    .unwrap();
    let bridge = std::sync::Arc::new(bridge);

    let start = std::time::Instant::now();
    let callers: Vec<_> = (0..4)
        .map(|i| {
            let b = std::sync::Arc::clone(&bridge);
            std::thread::spawn(move || {
                let res = b.call(&format!("tool.{i}"), &json!({})).unwrap();
                assert_eq!(res["ok"], json!(true));
            })
        })
        .collect();
    for c in callers {
        c.join().unwrap();
    }
    let elapsed = start.elapsed();
    // Serial floor: 4 × 300 ms = 1200 ms. Concurrent reality: ~320 ms.
    assert!(
        elapsed < Duration::from_millis(900),
        "concurrent round-trips serialized behind each other: {elapsed:?}"
    );
}

/// `kill_child` recycles a wedged generation immediately while callers still
/// hold clones: their already-written in-flight requests surface as the
/// AMBIGUOUS `helper_exited` (never unsent, never a silent hang).
#[test]
fn kill_child_fails_inflight_calls_as_ambiguous_exit() {
    let scenario = scenario_file(&json!({
        "echo": true,
        "sleep_ms_per_call": 1200,
    }));
    let bridge = std::sync::Arc::new(
        Bridge::spawn_args(
            fake_helper(),
            &[scenario.path().to_string_lossy().to_string()],
        )
        .unwrap(),
    );

    let caller = {
        let b = std::sync::Arc::clone(&bridge);
        std::thread::spawn(move || b.call_timeout("slow.tool", &json!({}), Duration::from_secs(8)))
    };
    std::thread::sleep(Duration::from_millis(250)); // request is written, helper busy
    bridge.kill_child();
    let err = caller.join().unwrap().unwrap_err();
    assert_eq!(err.code, "helper_exited");
}
