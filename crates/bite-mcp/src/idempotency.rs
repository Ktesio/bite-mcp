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
//! Requests are served one thread per call, so "is this key free?" and "a
//! call is dispatching under it" cannot be two separate lock holds: a
//! lookup-then-dispatch-then-record sequence would let two concurrent
//! same-key requests both pass the empty lookup and both reach the helper
//! — exactly the retry pattern the key exists to make safe. `reserve`
//! closes that check-then-act window by looking the key up and marking it
//! in-flight under ONE lock; the concurrent loser is refused with
//! `idempotency_in_flight` until the winner settles.
//!
//! The key never crosses the bridge: it is stripped from the arguments
//! before the call is forwarded, so the Swift helper's contract is
//! unchanged.

use std::collections::{HashMap, VecDeque};

use serde_json::Value;

/// The declared registry param name.
pub(crate) const KEY_PARAM: &str = "idempotency_key";

/// Keys retained per server process. Beyond this the oldest settled key is
/// evicted, completed outcomes before unknown pins; in-flight keys are
/// never evicted (see [`Idempotency::reserve`]).
const MAX_KEYS: usize = 128;

/// Longest accepted key — longer client keys are a client bug, not a
/// curiosity to store.
pub(crate) const MAX_KEY_LEN: usize = 128;

#[derive(Debug)]
pub(crate) enum Recorded {
    /// The call completed; repeated calls replay this value verbatim.
    Done(Value),
    /// The attempt failed ambiguously (it may have committed); repeated
    /// calls are refused. Carries the failure code for the refusal message.
    Unknown(String),
    /// A call with this key is currently in flight in another request
    /// thread; repeats are refused until it settles. Never an eviction
    /// victim — the owning thread still owes a `settle`/`release`.
    InFlight,
}

/// The result of atomically claiming a key via [`Idempotency::reserve`].
#[derive(Debug)]
pub(crate) enum Reserved {
    /// The key was free and is now marked in-flight for THIS caller, which
    /// must later `settle` or `release` it.
    Fresh,
    /// A completed outcome to replay verbatim.
    Done(Value),
    /// An earlier attempt failed ambiguously; carries its failure code.
    Unknown(String),
    /// Another request thread holds the key right now.
    InFlight,
}

/// How the owning thread ends a reservation — the two outcomes the call
/// site already distinguishes.
#[derive(Debug)]
pub(crate) enum Settled {
    /// The call completed with this value.
    Done(Value),
    /// The call failed ambiguously (carries the failure code).
    Unknown(String),
}

#[derive(Default)]
pub(crate) struct Idempotency {
    /// Retention order, oldest first — drives eviction.
    order: VecDeque<String>,
    map: HashMap<String, Recorded>,
}

impl Idempotency {
    /// Atomically look the key up and, when it has no recorded state, mark
    /// it in-flight: the single lock hold closes the check-then-act window
    /// between "key free" and "call dispatching", so two concurrent
    /// same-key requests cannot both reach the helper — the loser gets
    /// [`Reserved::InFlight`] instead.
    pub(crate) fn reserve(&mut self, key: &str) -> Reserved {
        match self.map.get(key) {
            Some(Recorded::Done(value)) => return Reserved::Done(value.clone()),
            Some(Recorded::Unknown(code)) => return Reserved::Unknown(code.clone()),
            Some(Recorded::InFlight) => return Reserved::InFlight,
            None => {}
        }
        self.evict_if_full();
        self.order.push_back(key.to_string());
        self.map.insert(key.to_string(), Recorded::InFlight);
        Reserved::Fresh
    }

    /// Settle an in-flight reservation with a final outcome. The order
    /// entry created by `reserve` is kept — the map entry is overwritten in
    /// place, never duplicated.
    pub(crate) fn settle(&mut self, key: &str, outcome: Settled) {
        let recorded = match outcome {
            Settled::Done(value) => Recorded::Done(value),
            Settled::Unknown(code) => Recorded::Unknown(code),
        };
        let prev = self.map.insert(key.to_string(), recorded);
        // Only the owning thread settles its own reservation.
        debug_assert!(
            matches!(prev, Some(Recorded::InFlight)),
            "settle must follow reserve"
        );
    }

    /// Release an in-flight reservation WITHOUT recording an outcome: the
    /// attempt failed cleanly (provably nothing was sent), so the key
    /// returns to unset and an immediate retry with the same key stays
    /// possible.
    pub(crate) fn release(&mut self, key: &str) {
        if matches!(self.map.get(key), Some(Recorded::InFlight)) {
            self.map.remove(key);
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                self.order.remove(pos);
            }
        }
    }

    /// Beyond the cap, evict the oldest settled key — a completed outcome
    /// (`Done`) before an unknown pin (`Unknown`): the pin is the state
    /// that guards against a double send, so it outlives replays whenever
    /// there is a choice. In-flight keys are never evicted; if EVERY slot
    /// is in flight the bound is soft for that transient window (each of
    /// those threads owes a settle/release that brings the count back).
    fn evict_if_full(&mut self) {
        if self.order.len() < MAX_KEYS {
            return;
        }
        let victim = self
            .order
            .iter()
            .position(|k| matches!(self.map.get(k), Some(Recorded::Done(_))))
            .or_else(|| {
                self.order
                    .iter()
                    .position(|k| matches!(self.map.get(k), Some(Recorded::Unknown(_))))
            });
        if let Some(pos) = victim {
            let evicted = self.order.remove(pos).expect("position came from order");
            self.map.remove(&evicted);
        }
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
        return Err(format!("{KEY_PARAM} must be at most {MAX_KEY_LEN} bytes"));
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
        let message = take_key(&mut v).unwrap_err();
        assert!(message.contains("at most"), "got: {message}");
        assert!(
            message.contains("bytes"),
            "the limit is measured in bytes (str::len), got: {message}"
        );
    }

    #[test]
    fn reserve_claims_fresh_and_refuses_concurrent_same_key() {
        let mut idem = Idempotency::default();
        assert!(matches!(idem.reserve("k"), Reserved::Fresh));
        // A second claim while the first is still in flight is refused —
        // this is the check-then-act race the single lock hold closes.
        assert!(matches!(idem.reserve("k"), Reserved::InFlight));
        idem.settle("k", Settled::Done(json!({ "sent": true })));
        // Once settled, repeats replay the recorded outcome instead.
        match idem.reserve("k") {
            Reserved::Done(v) => assert_eq!(v, json!({ "sent": true })),
            other => panic!("expected replay after settle, got {other:?}"),
        }
    }

    #[test]
    fn release_frees_a_cleanly_failed_key_for_retry() {
        let mut idem = Idempotency::default();
        assert!(matches!(idem.reserve("k"), Reserved::Fresh));
        idem.release("k");
        assert!(
            matches!(idem.reserve("k"), Reserved::Fresh),
            "a clean failure must free the key for an immediate retry"
        );
    }

    #[test]
    fn settle_keeps_order_and_map_in_lockstep() {
        let mut idem = Idempotency::default();
        assert!(matches!(idem.reserve("k"), Reserved::Fresh));
        idem.settle("k", Settled::Unknown("timeout".into()));
        assert_eq!(idem.order.len(), 1);
        assert_eq!(idem.map.len(), 1);
        // A stray release after settling must NOT unpin the key: only an
        // in-flight claim is releasable (production releases exactly the
        // key it reserved, before any settle).
        idem.release("k");
        assert!(
            matches!(idem.reserve("k"), Reserved::Unknown(_)),
            "release on a settled key is a no-op"
        );
    }

    #[test]
    fn eviction_prefers_done_before_unknown_and_never_in_flight() {
        let mut idem = Idempotency::default();
        // k0 pinned unknown (ambiguous), k1..=k{MAX_KEYS-1} done, plus one
        // key left in flight.
        idem.reserve("k0");
        idem.settle("k0", Settled::Unknown("helper_exited".into()));
        for i in 1..MAX_KEYS {
            let key = format!("k{i}");
            idem.reserve(&key);
            idem.settle(&key, Settled::Done(json!({ "i": i })));
        }
        assert!(matches!(idem.reserve("k-live"), Reserved::Fresh));

        // Cap reached: the next reserve evicts the OLDEST Done (k1) — not
        // the older unknown pin (k0), never the in-flight key.
        assert!(matches!(idem.reserve("k-new"), Reserved::Fresh));
        match idem.reserve("k0") {
            Reserved::Unknown(code) => assert_eq!(code, "helper_exited"),
            other => panic!("unknown pin must outlive settled sends, got {other:?}"),
        }
        assert!(
            matches!(idem.reserve("k1"), Reserved::Fresh),
            "the oldest completed outcome is the eviction victim"
        );
        assert!(
            matches!(idem.reserve("k-live"), Reserved::InFlight),
            "in-flight keys are never evicted"
        );
        // The reserved-not-yet-settled key is still in flight; once its
        // owner settles, repeats replay.
        assert!(matches!(idem.reserve("k-new"), Reserved::InFlight));
        idem.settle("k-new", Settled::Done(json!({ "n": 1 })));
        match idem.reserve("k-new") {
            Reserved::Done(v) => assert_eq!(v, json!({ "n": 1 })),
            other => panic!("expected replay after settle, got {other:?}"),
        }
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
