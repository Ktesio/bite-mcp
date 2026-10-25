//! Client-supplied idempotency keys for the mutating send path.
//!
//! The transport-level guard in `mcp::run_tool_retrying` correctly refuses
//! to blind-replay non-idempotent tools after an ambiguous helper failure
//! (mid-call death or a wedged call recycled on timeout): the first attempt
//! may already have committed, and a replay could double-send. But a client
//! that never got a response then has no safe way to retry at all. An
//! optional client-chosen `idempotency_key` — declared on the tools that
//! opt in (`mail_send` today) — closes that gap server-side: the first
//! completed attempt's outcome is recorded against the key, and a repeated
//! call with the same key replays the recorded outcome verbatim instead of
//! dispatching a second send. An attempt that failed AMBIGUOUSLY pins the
//! key as unknown: repeated calls are refused with a specific error rather
//! than risking a double send (the client confirms the original outcome and
//! uses a fresh key). Clean failures — `helper_exited_unsent` (provably
//! never reached the helper) and tool-level errors returned by a healthy
//! helper — do not pin the key: the send provably did not go out, so an
//! immediate retry with the same key stays possible.
//!
//! The key never crosses the bridge: it is stripped from the arguments
//! before the call is forwarded, so the Swift helper's contract is
//! unchanged.

use std::collections::{HashMap, VecDeque};

use serde_json::Value;

/// The declared registry param name.
pub(crate) const KEY_PARAM: &str = "idempotency_key";

/// Keys retained per server process; the oldest is evicted beyond this.
const MAX_KEYS: usize = 128;

/// Longest accepted key — longer client keys are a client bug, not a
/// curiosity to store.
const MAX_KEY_LEN: usize = 128;

pub(crate) enum Recorded {
    /// The call completed; repeated calls replay this value verbatim.
    Done(Value),
    /// The attempt failed ambiguously (it may have committed); repeated
    /// calls are refused. Carries the failure code for the refusal message.
    Unknown(String),
}

#[derive(Default)]
pub(crate) struct Idempotency {
    /// Insertion order, oldest first — drives eviction.
    order: VecDeque<String>,
    map: HashMap<String, Recorded>,
}

impl Idempotency {
    pub(crate) fn lookup(&self, key: &str) -> Option<&Recorded> {
        self.map.get(key)
    }

    /// Record the outcome for `key`. A key is only ever recorded once per
    /// process: `lookup` intercepts repeats before any call is dispatched,
    /// so this is a plain insert with oldest-first eviction.
    pub(crate) fn record(&mut self, key: String, outcome: Recorded) {
        if self.order.len() >= MAX_KEYS {
            if let Some(evicted) = self.order.pop_front() {
                self.map.remove(&evicted);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, outcome);
    }
}

/// Extract and validate the key from tool-call arguments, REMOVING it so
/// the stripped arguments can be forwarded to the helper unchanged.
/// `Ok(None)`: absent, or JSON null (treated as absent, the optional-param
/// convention elsewhere in the surface).
pub(crate) fn take_key(args: &mut Value) -> Result<Option<String>, String> {
    let Some(obj) = args.as_object_mut() else {
        return Ok(None); // non-object arguments never carry a key
    };
    if !obj.contains_key(KEY_PARAM) {
        return Ok(None);
    }
    let raw = obj.remove(KEY_PARAM).expect("checked above");
    if raw.is_null() {
        return Ok(None);
    }
    let Some(key) = raw.as_str().map(str::to_string) else {
        return Err(format!("{KEY_PARAM} must be a string"));
    };
    if key.is_empty() {
        return Err(format!("{KEY_PARAM} must not be empty"));
    }
    if key.len() > MAX_KEY_LEN {
        return Err(format!("{KEY_PARAM} must be at most {MAX_KEY_LEN} chars"));
    }
    Ok(Some(key))
}

/// Failure codes whose outcome is genuinely ambiguous — the attempt may
/// have committed inside the helper, so the key is pinned as unknown.
/// `helper_exited_unsent` provably never reached the helper and tool-level
/// errors provably did not send; both leave the key free for a retry.
pub(crate) fn ambiguous(code: &str) -> bool {
    matches!(code, "helper_exited" | "timeout")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn take_key_strips_and_validates() {
        // absent / null → None, other args untouched
        let mut v = json!({ "to": ["a@b.c"], "subject": "hi" });
        assert_eq!(take_key(&mut v).unwrap(), None);
        let mut v = json!({ "to": ["a@b.c"], "idempotency_key": null });
        assert_eq!(take_key(&mut v).unwrap(), None);
        assert!(
            v.get("idempotency_key").is_none(),
            "null key must be removed too"
        );

        // valid key → Some, stripped from the forwarded args
        let mut v = json!({ "to": ["a@b.c"], "idempotency_key": "k-1" });
        assert_eq!(take_key(&mut v).unwrap().as_deref(), Some("k-1"));
        assert_eq!(
            v,
            json!({ "to": ["a@b.c"] }),
            "key must not reach the helper"
        );

        // non-object arguments never carry a key
        assert_eq!(take_key(&mut Value::Null).unwrap(), None);

        // rejections
        let mut v = json!({ "idempotency_key": 7 });
        assert!(take_key(&mut v).unwrap_err().contains("must be a string"));
        let mut v = json!({ "idempotency_key": "" });
        assert!(take_key(&mut v).unwrap_err().contains("must not be empty"));
        let mut v = json!({ "idempotency_key": "x".repeat(MAX_KEY_LEN + 1) });
        assert!(take_key(&mut v).unwrap_err().contains("at most"));
    }

    #[test]
    fn record_evicts_oldest_beyond_cap() {
        let mut idem = Idempotency::default();
        for i in 0..MAX_KEYS {
            idem.record(format!("k{i}"), Recorded::Done(json!({ "i": i })));
        }
        assert!(idem.lookup("k0").is_some());
        idem.record("k-new".to_string(), Recorded::Done(json!({})));
        assert!(idem.lookup("k0").is_none(), "oldest key must be evicted");
        assert!(idem.lookup("k1").is_some());
        assert!(idem.lookup("k-new").is_some());
    }

    #[test]
    fn ambiguous_covers_exactly_the_two_unresolved_codes() {
        assert!(ambiguous("helper_exited"));
        assert!(ambiguous("timeout"));
        assert!(!ambiguous("helper_exited_unsent"));
        assert!(!ambiguous("invalid_params"));
        assert!(!ambiguous("permission_denied"));
    }
}
