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
//! never reached the helper), tool-level errors returned by a healthy
//! helper, and spawn/handshake failures that never produced a bridge at
//! all — do not pin the key: the send provably did not go out, so an
//! immediate retry with the same key stays possible.
//!
//! A replay is only honest when the repeat call would send the SAME mail:
//! each claim records a fixed-size digest of the canonical form of the
//! stripped arguments (see [`fingerprint`]), and a key reused with
//! DIFFERENT arguments is refused outright (`idempotency_key_reused`) —
//! replaying would return the first call's outcome while the second mail
//! is silently never sent.
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

use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};

use serde_json::Value;

/// The declared registry param name.
pub(crate) const KEY_PARAM: &str = "idempotency_key";

/// Fixed-size argument fingerprint stored per retained key: SHA-256 over
/// the canonical argument text, so every entry costs 32 bytes regardless
/// of mail body size and the MAX_KEYS cap bounds TOTAL retention memory.
pub(crate) type Fingerprint = [u8; 32];

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

/// One key's recorded state plus the fixed-size argument fingerprint of
/// the stripped arguments its claim was made with (see [`fingerprint`]).
/// A repeat whose fingerprint differs is refused as a reuse instead of
/// replayed.
#[derive(Debug)]
struct Entry {
    state: Recorded,
    fingerprint: Fingerprint,
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
    /// The key is already recorded (or claimed) for a call with DIFFERENT
    /// arguments: replaying would silently discard this call's arguments,
    /// so the repeat is refused instead. The recorded entry is untouched.
    Reused,
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
    map: HashMap<String, Entry>,
}

impl Idempotency {
    /// Atomically look the key up and, when it has no recorded state, mark
    /// it in-flight: the single lock hold closes the check-then-act window
    /// between "key free" and "call dispatching", so two concurrent
    /// same-key requests cannot both reach the helper — the loser gets
    /// [`Reserved::InFlight`] instead. `fingerprint` is the canonical form
    /// of the stripped arguments THIS call would dispatch with; a recorded
    /// (or in-flight) entry claimed under different arguments yields
    /// [`Reserved::Reused`] — a replay would silently discard this call's
    /// arguments, so it is refused rather than replayed.
    pub(crate) fn reserve(&mut self, key: &str, fingerprint: &Fingerprint) -> Reserved {
        if let Some(entry) = self.map.get(key) {
            if entry.fingerprint != *fingerprint {
                return Reserved::Reused;
            }
            return match &entry.state {
                Recorded::Done(value) => Reserved::Done(value.clone()),
                Recorded::Unknown(code) => Reserved::Unknown(code.clone()),
                Recorded::InFlight => Reserved::InFlight,
            };
        }
        self.evict_if_full();
        self.order.push_back(key.to_string());
        self.map.insert(
            key.to_string(),
            Entry {
                state: Recorded::InFlight,
                fingerprint: *fingerprint,
            },
        );
        Reserved::Fresh
    }

    /// Settle an in-flight reservation with a final outcome. The order
    /// entry created by `reserve` is kept — the entry is updated in place,
    /// never duplicated, and the fingerprint it was claimed under stays.
    pub(crate) fn settle(&mut self, key: &str, outcome: Settled) {
        let state = match outcome {
            Settled::Done(value) => Recorded::Done(value),
            Settled::Unknown(code) => Recorded::Unknown(code),
        };
        match self.map.get_mut(key) {
            // Only the owning thread settles its own reservation.
            Some(entry) => entry.state = state,
            None => debug_assert!(false, "settle must follow reserve"),
        }
    }

    /// Release an in-flight reservation WITHOUT recording an outcome: the
    /// attempt failed cleanly (provably nothing was sent), so the key
    /// returns to unset and an immediate retry with the same key stays
    /// possible — with ANY arguments: no outcome was recorded, so there is
    /// nothing for new arguments to desynchronize.
    pub(crate) fn release(&mut self, key: &str) {
        if matches!(
            self.map.get(key),
            Some(Entry {
                state: Recorded::InFlight,
                ..
            })
        ) {
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
            .position(|k| {
                matches!(
                    self.map.get(k),
                    Some(Entry {
                        state: Recorded::Done(_),
                        ..
                    })
                )
            })
            .or_else(|| {
                self.order.iter().position(|k| {
                    matches!(
                        self.map.get(k),
                        Some(Entry {
                            state: Recorded::Unknown(_),
                            ..
                        })
                    )
                })
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

/// Fixed-size fingerprint of the STRIPPED arguments (the key is already
/// gone) that a keyed call dispatches with: SHA-256 over the canonical
/// text — JSON with every object's keys sorted. Raw `serde_json::to_string`
/// is NOT canonical in this workspace — the `preserve_order` feature rides
/// into the graph through the dependency stack, so the map serializes in
/// insertion order and the same call serialized twice with different key
/// order would fingerprint apart. Sorting makes two argument objects that
/// differ only in key order fingerprint identically, while any difference
/// in the values the helper would see still fingerprints differently; the
/// canonical text is streamed straight into the hasher (never materialized
/// as one string), so the stored fingerprint is always 32 bytes regardless
/// of mail body size and the MAX_KEYS retention cap bounds total memory. A
/// key reused under a different fingerprint is refused instead of
/// replayed. (sha2 is already in the dependency graph via bite-index.)
pub(crate) fn fingerprint(stripped_args: &Value) -> Fingerprint {
    let mut hasher = Sha256::new();
    let mut push = |chunk: &str| hasher.update(chunk.as_bytes());
    canonical_json(stripped_args, &mut push);
    hasher.finalize().into()
}

/// Write the canonical form of a value — JSON text with object keys sorted
/// (arrays keep their order — JSON arrays are ordered by definition) — into
/// `push` in chunks. Scalars reuse serde_json's own rendering.
fn canonical_json(value: &Value, push: &mut impl FnMut(&str)) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            push("{");
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    push(",");
                }
                push(&serde_json::to_string(key).expect("str serializes"));
                push(":");
                canonical_json(&map[*key], push);
            }
            push("}");
        }
        Value::Array(items) => {
            push("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    push(",");
                }
                canonical_json(item, push);
            }
            push("]");
        }
        scalar => push(&serde_json::to_string(scalar).expect("a Value serializes")),
    }
}

/// Failure codes whose outcome is genuinely ambiguous WHEN A REQUEST WAS
/// WRITTEN — the attempt may have committed inside the helper, so the key
/// is pinned as unknown. The same codes are reused by a failed
/// spawn/handshake (no bridge was ever produced), where provably nothing
/// was written: callers must additionally require the request-written flag
/// from `run_tool_retrying` (see `ToolRun`) before pinning on these codes.
/// `helper_exited_unsent` provably never reached the helper and tool-level
/// errors provably did not send; both leave the key free for a retry.
pub(crate) fn ambiguous(code: &str) -> bool {
    matches!(code, "helper_exited" | "timeout")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Test fingerprints: the real `fingerprint()` over a JSON scalar of
    /// the label — production always digests the stripped-argument object,
    /// but any stable digest serves the state-machine tests below.
    fn fp(label: &str) -> Fingerprint {
        fingerprint(&json!(label))
    }

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
        assert!(matches!(idem.reserve("k", &fp("args")), Reserved::Fresh));
        // A second claim while the first is still in flight is refused —
        // this is the check-then-act race the single lock hold closes.
        assert!(matches!(idem.reserve("k", &fp("args")), Reserved::InFlight));
        idem.settle("k", Settled::Done(json!({ "sent": true })));
        // Once settled, repeats replay the recorded outcome instead.
        match idem.reserve("k", &fp("args")) {
            Reserved::Done(v) => assert_eq!(v, json!({ "sent": true })),
            other => panic!("expected replay after settle, got {other:?}"),
        }
    }

    #[test]
    fn release_frees_a_cleanly_failed_key_for_retry() {
        let mut idem = Idempotency::default();
        assert!(matches!(idem.reserve("k", &fp("args")), Reserved::Fresh));
        idem.release("k");
        assert!(
            matches!(idem.reserve("k", &fp("args")), Reserved::Fresh),
            "a clean failure must free the key for an immediate retry"
        );
    }

    #[test]
    fn settle_keeps_order_and_map_in_lockstep() {
        let mut idem = Idempotency::default();
        assert!(matches!(idem.reserve("k", &fp("args")), Reserved::Fresh));
        idem.settle("k", Settled::Unknown("timeout".into()));
        assert_eq!(idem.order.len(), 1);
        assert_eq!(idem.map.len(), 1);
        // A stray release after settling must NOT unpin the key: only an
        // in-flight claim is releasable (production releases exactly the
        // key it reserved, before any settle).
        idem.release("k");
        assert!(
            matches!(idem.reserve("k", &fp("args")), Reserved::Unknown(_)),
            "release on a settled key is a no-op"
        );
    }

    #[test]
    fn reused_key_with_different_arguments_is_refused() {
        let mut idem = Idempotency::default();
        assert!(matches!(idem.reserve("k", &fp("args-a")), Reserved::Fresh));
        idem.settle("k", Settled::Done(json!({ "sent": true })));
        // Identical arguments: the recorded outcome replays.
        match idem.reserve("k", &fp("args-a")) {
            Reserved::Done(v) => assert_eq!(v, json!({ "sent": true })),
            other => panic!("an identical repeat must replay, got {other:?}"),
        }
        // Different arguments: refused — a replay would return the old
        // outcome while this call's mail is silently never sent.
        assert!(
            matches!(idem.reserve("k", &fp("args-b")), Reserved::Reused),
            "a key reused with different arguments must be refused, not replayed"
        );
        // The refusal leaves the recording untouched: an identical repeat
        // still replays afterwards.
        match idem.reserve("k", &fp("args-a")) {
            Reserved::Done(v) => assert_eq!(v, json!({ "sent": true })),
            other => panic!("the recording must survive a refused reuse, got {other:?}"),
        }
    }

    #[test]
    fn reuse_refusal_covers_unknown_pins_and_in_flight_claims() {
        let mut idem = Idempotency::default();
        // Unknown pin claimed under args-a: different arguments are refused
        // as a reuse — WHICH arguments were pinned matters, not just that
        // the key is pinned.
        idem.reserve("u", &fp("args-a"));
        idem.settle("u", Settled::Unknown("timeout".into()));
        assert!(matches!(idem.reserve("u", &fp("args-b")), Reserved::Reused));
        // The pin survives the refused reuse for the original arguments.
        match idem.reserve("u", &fp("args-a")) {
            Reserved::Unknown(code) => assert_eq!(code, "timeout"),
            other => panic!("expected the unknown pin to survive, got {other:?}"),
        }
        // In flight: a different-arguments repeat is a reuse refusal (the
        // argument mismatch is the stronger fact); identical repeats still
        // see InFlight.
        idem.reserve("i", &fp("args-a"));
        assert!(matches!(idem.reserve("i", &fp("args-b")), Reserved::Reused));
        assert!(matches!(
            idem.reserve("i", &fp("args-a")),
            Reserved::InFlight
        ));
        // A cleanly released key is free for ANY arguments — nothing was
        // recorded, so new arguments have nothing to desynchronize.
        idem.reserve("r", &fp("args-a"));
        idem.release("r");
        assert!(matches!(idem.reserve("r", &fp("args-b")), Reserved::Fresh));
    }

    #[test]
    fn fingerprint_orders_object_keys_canonically() {
        // Two argument objects that differ only in key order describe the
        // same call and must fingerprint identically (the fingerprint
        // sorts object keys — raw serde_json text is insertion-ordered in
        // this workspace via preserve_order); any value difference must
        // fingerprint apart. Nested objects sort too.
        let a = json!({ "to": ["a@b.c"], "subject": "hi" });
        let b = json!({ "subject": "hi", "to": ["a@b.c"] });
        assert_eq!(fingerprint(&a), fingerprint(&b));
        let c = json!({ "to": ["a@b.c"], "subject": "hi there" });
        assert_ne!(fingerprint(&a), fingerprint(&c));
        let nested_a = json!({ "filter": { "from": "x@y", "folder": "Inbox" } });
        let nested_b = json!({ "filter": { "folder": "Inbox", "from": "x@y" } });
        assert_eq!(fingerprint(&nested_a), fingerprint(&nested_b));
        // Array order is significant — recipients in a different order are
        // a different call.
        let arr_a = json!({ "to": ["a@b.c", "d@e.f"] });
        let arr_b = json!({ "to": ["d@e.f", "a@b.c"] });
        assert_ne!(fingerprint(&arr_a), fingerprint(&arr_b));
    }

    #[test]
    fn fingerprint_is_fixed_size_regardless_of_payload() {
        // The retention cap bounds the NUMBER of entries; storing raw
        // argument text would leave per-entry memory proportional to the
        // mail body (128 keyed sends with 2 MB bodies would retain ~256 MB
        // of fingerprint strings in the long-lived server). The digest
        // bounds each entry at 32 bytes no matter the payload.
        let small = fingerprint(&json!({ "to": ["a@b.c"], "body": "hi" }));
        let big = fingerprint(&json!({
            "to": ["a@b.c"],
            "body": "x".repeat(2_000_000)
        }));
        assert_eq!(small.len(), 32);
        assert_eq!(big.len(), 32, "digest size must not grow with the body");
        assert_ne!(small, big, "the digest must still distinguish payloads");
    }

    #[test]
    fn eviction_prefers_done_before_unknown_and_never_in_flight() {
        let mut idem = Idempotency::default();
        // k0 pinned unknown (ambiguous), k1..=k{MAX_KEYS-1} done, plus one
        // key left in flight.
        idem.reserve("k0", &fp("args"));
        idem.settle("k0", Settled::Unknown("helper_exited".into()));
        for i in 1..MAX_KEYS {
            let key = format!("k{i}");
            idem.reserve(&key, &fp("args"));
            idem.settle(&key, Settled::Done(json!({ "i": i })));
        }
        assert!(matches!(
            idem.reserve("k-live", &fp("args")),
            Reserved::Fresh
        ));

        // Cap reached: the next reserve evicts the OLDEST Done (k1) — not
        // the older unknown pin (k0), never the in-flight key.
        assert!(matches!(
            idem.reserve("k-new", &fp("args")),
            Reserved::Fresh
        ));
        match idem.reserve("k0", &fp("args")) {
            Reserved::Unknown(code) => assert_eq!(code, "helper_exited"),
            other => panic!("unknown pin must outlive settled sends, got {other:?}"),
        }
        assert!(
            matches!(idem.reserve("k1", &fp("args")), Reserved::Fresh),
            "the oldest completed outcome is the eviction victim"
        );
        assert!(
            matches!(idem.reserve("k-live", &fp("args")), Reserved::InFlight),
            "in-flight keys are never evicted"
        );
        // The reserved-not-yet-settled key is still in flight; once its
        // owner settles, repeats replay.
        assert!(matches!(
            idem.reserve("k-new", &fp("args")),
            Reserved::InFlight
        ));
        idem.settle("k-new", Settled::Done(json!({ "n": 1 })));
        match idem.reserve("k-new", &fp("args")) {
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
