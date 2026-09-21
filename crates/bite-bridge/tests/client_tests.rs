//! Golden-conversation tests against the scripted fake helper.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use bite_bridge::{Bridge, BridgeError};
use serde_json::json;

fn fake_helper() -> &'static str {
    env!("CARGO_BIN_EXE_bite-fake-helper")
}

fn scenario_file(scenario: &serde_json::Value) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "bite-fake-scenario-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, serde_json::to_string(scenario).unwrap()).unwrap();
    path
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
    let bridge = Bridge::spawn_args(fake_helper(), &[path.display().to_string()]).unwrap();
    let err: BridgeError = bridge.call("mail.accounts", &json!({})).unwrap_err();
    assert!(err.is_permission());
    assert_eq!(err.app.as_deref(), Some("Mail"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn timeout_is_reported_and_bridge_survives() {
    let path = scenario_file(&json!({ "echo": true, "sleep_ms_per_call": 1500 }));
    let bridge = Bridge::spawn_args(fake_helper(), &[path.display().to_string()]).unwrap();
    let err = bridge
        .call_timeout("sys.ping", &json!({}), Duration::from_millis(200))
        .unwrap_err();
    assert_eq!(err.code, "timeout");
    // the slow response is discarded; next fast request still works
    let res = bridge
        .call_timeout("sys.ping", &json!({}), Duration::from_secs(5))
        .unwrap();
    assert_eq!(res["ok"], json!(true));
    let _ = std::fs::remove_file(path);
}

#[test]
fn helper_exit_fails_pending_requests() {
    // helper prints hello then exits: either spawn reports the exit, or the
    // first call fails with helper_exited — never a silent success.
    let path = scenario_file(&json!({ "exit_after_hello": true }));
    let err = match Bridge::spawn_args(fake_helper(), &[path.display().to_string()]) {
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
