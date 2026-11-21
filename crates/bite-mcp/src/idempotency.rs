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
//!
//! Settled records (completed outcomes and unknown pins) are DURABLE: they
//! are persisted to a 0600 `idempotency-ledger.json` in the 0700 data dir
//! (see [`Idempotency::persist_ledger`]) and reloaded on startup (see
//! [`load_ledger`]), so a server restart between a client's ambiguous
//! failure and its same-key retry no longer drops the pin — the exact
//! double-send window the key exists to close. The file stores SHA-256
//! hashes of the keys (never the client's key text) plus each entry's
//! argument fingerprint and settle time; entries expire from the ledger
//! after [`PERSIST_TTL_SECS`] and the on-disk set honors the same
//! [`MAX_KEYS`] cap as memory. In-flight claims are never persisted (a
//! restart provably kills the dispatching thread with them). A missing or
//! corrupt ledger starts the server unrecorded, with one stderr note.

use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::io::Write as _;

use serde_json::Value;

/// The declared registry param name.
pub(crate) const KEY_PARAM: &str = "idempotency_key";

/// Fixed-size argument fingerprint stored per retained key: SHA-256 over
/// the canonical argument text, so every entry costs 32 bytes regardless
/// of mail body size and the MAX_KEYS cap bounds TOTAL retention memory.
pub(crate) type Fingerprint = [u8; 32];

/// Fixed-size digest of the KEY ITSELF — the ledger's stored form of a
/// key. Client key text is user-controlled data; the file gets the same
/// SHA-256 treatment the fingerprints already use, so nothing the client
/// chose verbatim is written to disk.
pub(crate) type KeyHash = [u8; 32];

/// How long a settled ledger entry survives from its settle time — on disk
/// and after reload. Retries of an ambiguous send arrive within seconds; a
/// week bounds how long a replay/refusal answer can lag behind what the
/// client remembers without inviting stale-key surprises.
pub(crate) const PERSIST_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Ledger file name inside the (0700) data dir; written 0600.
pub(crate) const LEDGER_FILE: &str = "idempotency-ledger.json";

/// Keys retained per server process. Beyond this the oldest settled key is
/// evicted, completed outcomes before unknown pins; in-flight keys are
/// never evicted (see [`Idempotency::reserve`]). The reloaded ledger
/// honors the same cap (oldest entries dropped on load).
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
/// replayed. `settled_at` (unix seconds) is set when the entry settles and
/// rides the durable ledger; it is `None` only while in flight.
#[derive(Debug)]
struct Entry {
    state: Recorded,
    fingerprint: Fingerprint,
    settled_at: Option<u64>,
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

/// Current unix time in seconds (0 if the clock is somehow before the
/// epoch — degraded, but a fallback that only makes entries look fresh,
/// never expires them early).
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Digest of the key text — the ledger's stored form of a key.
pub(crate) fn key_hash(key: &str) -> KeyHash {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hasher.finalize().into()
}

/// Absolute path of the durable ledger in the private data dir.
pub(crate) fn ledger_path() -> std::path::PathBuf {
    bite_core::config::data_dir().join(LEDGER_FILE)
}

#[derive(Default)]
pub(crate) struct Idempotency {
    /// Retention order, oldest first — drives eviction.
    order: VecDeque<KeyHash>,
    map: HashMap<KeyHash, Entry>,
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
    pub(crate) fn reserve(&mut self, key: &KeyHash, fingerprint: &Fingerprint) -> Reserved {
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
        self.order.push_back(*key);
        self.map.insert(
            *key,
            Entry {
                state: Recorded::InFlight,
                fingerprint: *fingerprint,
                settled_at: None,
            },
        );
        Reserved::Fresh
    }

    /// Settle an in-flight reservation with a final outcome. The order
    /// entry created by `reserve` is kept — the entry is updated in place,
    /// never duplicated, and the fingerprint it was claimed under stays.
    pub(crate) fn settle(&mut self, key: &KeyHash, outcome: Settled) {
        let now = unix_now();
        let state = match outcome {
            Settled::Done(value) => Recorded::Done(value),
            Settled::Unknown(code) => Recorded::Unknown(code),
        };
        match self.map.get_mut(key) {
            // Only the owning thread settles its own reservation.
            Some(entry) => {
                entry.state = state;
                entry.settled_at = Some(now);
            }
            None => debug_assert!(false, "settle must follow reserve"),
        }
    }

    /// Settle and best-effort persist: the durable ledger must never turn a
    /// completed send into a client-visible failure (the client would
    /// retry and double-send), so a persist error is noted on stderr — the
    /// one operator-facing channel a stdio server has — and swallowed.
    /// Callers already hold the state lock; keeping settle + persist under
    /// that one hold means the file never serializes a half-updated table.
    pub(crate) fn settle_and_persist(
        &mut self,
        key: &KeyHash,
        outcome: Settled,
        path: &std::path::Path,
    ) {
        self.settle(key, outcome);
        if let Err(error) = self.persist_ledger(path) {
            let _ = writeln!(
                std::io::stderr(),
                "bite-mcp: idempotency ledger write failed ({error}); \
                 outcomes stay in memory for this process"
            );
        }
    }

    /// Release an in-flight reservation WITHOUT recording an outcome: the
    /// attempt failed cleanly (provably nothing was sent), so the key
    /// returns to unset and an immediate retry with the same key stays
    /// possible — with ANY arguments: no outcome was recorded, so there is
    /// nothing for new arguments to desynchronize. Nothing to persist: an
    /// in-flight claim was never written to the ledger.
    pub(crate) fn release(&mut self, key: &KeyHash) {
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

    /// Serialize every SETTLED entry (oldest first) into the durable
    /// ledger JSON and write it 0600 via the shared private-write helper
    /// (temp file + rename, so a crash never leaves a half-written
    /// table). In-flight claims are skipped — a restart kills the
    /// dispatching thread, and a stale in-flight row would refuse retries
    /// of a call that provably never landed.
    pub(crate) fn persist_ledger(&self, path: &std::path::Path) -> std::io::Result<()> {
        let entries: Vec<Value> = self
            .order
            .iter()
            .filter_map(|key| self.map.get(key).map(|entry| (*key, entry)))
            .filter_map(|(key, entry)| {
                let settled_at = entry.settled_at?;
                let record = match &entry.state {
                    Recorded::Done(value) => {
                        json_record(&key, entry, "done", Some(value), None, settled_at)
                    }
                    Recorded::Unknown(code) => {
                        json_record(&key, entry, "unknown", None, Some(code), settled_at)
                    }
                    Recorded::InFlight => return None,
                };
                Some(record)
            })
            .collect();
        let doc = serde_json::json!({ "v": 1, "entries": entries });
        bite_core::fsops::write_private(path, doc.to_string().as_bytes())
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

/// Reload the durable ledger. Expired entries (settle time older than
/// [`PERSIST_TTL_SECS`]) and malformed rows are dropped; surviving entries
/// keep their original settle times, so a reload never refreshes a dying
/// entry into another full TTL. The newest [`MAX_KEYS`] entries survive —
/// the same cap memory enforces. A missing file is the first-run case; a
/// corrupt file is noted on stderr (the operator channel) and otherwise
/// treated the same: the server starts unrecorded rather than refusing to
/// serve, trading a lost replay/refusal surface for availability.
pub(crate) fn load_ledger(path: &std::path::Path, now_secs: u64) -> Idempotency {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Idempotency::default(), // absent or unreadable: first run
    };
    let parsed: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            let _ = writeln!(
                std::io::stderr(),
                "bite-mcp: idempotency ledger is unreadable ({error}); starting unrecorded"
            );
            return Idempotency::default();
        }
    };
    let mut rows: Vec<(u64, KeyHash, Entry)> = Vec::new();
    let empty: Vec<Value> = Vec::new();
    let entries = parsed
        .get("entries")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for record in entries {
        let (Some(key), Some(fingerprint), Some(settled_at)) = (
            record.get("k").and_then(Value::as_str).and_then(unhex),
            record.get("f").and_then(Value::as_str).and_then(unhex),
            record.get("t").and_then(Value::as_u64),
        ) else {
            continue; // malformed row: drop it, keep the rest
        };
        if now_secs.saturating_sub(settled_at) > PERSIST_TTL_SECS {
            continue; // expired
        }
        let state = match record.get("s").and_then(Value::as_str) {
            Some("done") => match record.get("o").filter(|v| !v.is_null()) {
                Some(value) => Recorded::Done(value.clone()),
                None => continue, // a done row without its outcome is unusable
            },
            Some("unknown") => match record.get("c").and_then(Value::as_str) {
                Some(code) => Recorded::Unknown(code.to_string()),
                None => continue,
            },
            _ => continue, // unknown record kind (future version): drop
        };
        rows.push((
            settled_at,
            key,
            Entry {
                state,
                fingerprint,
                settled_at: Some(settled_at),
            },
        ));
    }
    rows.sort_by_key(|(settled_at, _, _)| *settled_at);
    let skip = rows.len().saturating_sub(MAX_KEYS);
    let mut idem = Idempotency::default();
    for (_, key, entry) in rows.into_iter().skip(skip) {
        idem.order.push_back(key);
        idem.map.insert(key, entry);
    }
    idem
}

/// One ledger row as JSON: hashed key, argument fingerprint, state kind,
/// outcome (done rows) or failure code (unknown rows), settle time (unix
/// seconds).
fn json_record(
    key: &KeyHash,
    entry: &Entry,
    kind: &str,
    outcome: Option<&Value>,
    code: Option<&str>,
    settled_at: u64,
) -> Value {
    serde_json::json!({
        "k": hex(key),
        "f": hex(&entry.fingerprint),
        "s": kind,
        "o": outcome,
        "c": code,
        "t": settled_at,
    })
}

/// Lowercase hex — the ledger's on-disk form for the two 32-byte digests.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Inverse of [`hex`] for a 32-byte digest; `None` on length or digit
/// mismatch (malformed rows are dropped by the loader, not fatal).
fn unhex(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = (bytes[2 * i] as char).to_digit(16)?;
        let lo = (bytes[2 * i + 1] as char).to_digit(16)?;
        *slot = (hi * 16 + lo) as u8;
    }
    Some(out)
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

    /// Test key hashes: the real `key_hash()` of the label.
    fn kh(label: &str) -> KeyHash {
        key_hash(label)
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
        assert!(matches!(
            idem.reserve(&kh("k"), &fp("args")),
            Reserved::Fresh
        ));
        // A second claim while the first is still in flight is refused —
        // this is the check-then-act race the single lock hold closes.
        assert!(matches!(
            idem.reserve(&kh("k"), &fp("args")),
            Reserved::InFlight
        ));
        idem.settle(&kh("k"), Settled::Done(json!({ "sent": true })));
        // Once settled, repeats replay the recorded outcome instead.
        match idem.reserve(&kh("k"), &fp("args")) {
            Reserved::Done(v) => assert_eq!(v, json!({ "sent": true })),
            other => panic!("expected replay after settle, got {other:?}"),
        }
    }

    #[test]
    fn release_frees_a_cleanly_failed_key_for_retry() {
        let mut idem = Idempotency::default();
        assert!(matches!(
            idem.reserve(&kh("k"), &fp("args")),
            Reserved::Fresh
        ));
        idem.release(&kh("k"));
        assert!(
            matches!(idem.reserve(&kh("k"), &fp("args")), Reserved::Fresh),
            "a clean failure must free the key for an immediate retry"
        );
    }

    #[test]
    fn settle_keeps_order_and_map_in_lockstep() {
        let mut idem = Idempotency::default();
        assert!(matches!(
            idem.reserve(&kh("k"), &fp("args")),
            Reserved::Fresh
        ));
        idem.settle(&kh("k"), Settled::Unknown("timeout".into()));
        assert_eq!(idem.order.len(), 1);
        assert_eq!(idem.map.len(), 1);
        // A stray release after settling must NOT unpin the key: only an
        // in-flight claim is releasable (production releases exactly the
        // key it reserved, before any settle).
        idem.release(&kh("k"));
        assert!(
            matches!(idem.reserve(&kh("k"), &fp("args")), Reserved::Unknown(_)),
            "release on a settled key is a no-op"
        );
    }

    #[test]
    fn reused_key_with_different_arguments_is_refused() {
        let mut idem = Idempotency::default();
        assert!(matches!(
            idem.reserve(&kh("k"), &fp("args-a")),
            Reserved::Fresh
        ));
        idem.settle(&kh("k"), Settled::Done(json!({ "sent": true })));
        // Identical arguments: the recorded outcome replays.
        match idem.reserve(&kh("k"), &fp("args-a")) {
            Reserved::Done(v) => assert_eq!(v, json!({ "sent": true })),
            other => panic!("an identical repeat must replay, got {other:?}"),
        }
        // Different arguments: refused — a replay would return the old
        // outcome while this call's mail is silently never sent.
        assert!(
            matches!(idem.reserve(&kh("k"), &fp("args-b")), Reserved::Reused),
            "a key reused with different arguments must be refused, not replayed"
        );
        // The refusal leaves the recording untouched: an identical repeat
        // still replays afterwards.
        match idem.reserve(&kh("k"), &fp("args-a")) {
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
        idem.reserve(&kh("u"), &fp("args-a"));
        idem.settle(&kh("u"), Settled::Unknown("timeout".into()));
        assert!(matches!(
            idem.reserve(&kh("u"), &fp("args-b")),
            Reserved::Reused
        ));
        // The pin survives the refused reuse for the original arguments.
        match idem.reserve(&kh("u"), &fp("args-a")) {
            Reserved::Unknown(code) => assert_eq!(code, "timeout"),
            other => panic!("expected the unknown pin to survive, got {other:?}"),
        }
        // In flight: a different-arguments repeat is a reuse refusal (the
        // argument mismatch is the stronger fact); identical repeats still
        // see InFlight.
        idem.reserve(&kh("i"), &fp("args-a"));
        assert!(matches!(
            idem.reserve(&kh("i"), &fp("args-b")),
            Reserved::Reused
        ));
        assert!(matches!(
            idem.reserve(&kh("i"), &fp("args-a")),
            Reserved::InFlight
        ));
        // A cleanly released key is free for ANY arguments — nothing was
        // recorded, so new arguments have nothing to desynchronize.
        idem.reserve(&kh("r"), &fp("args-a"));
        idem.release(&kh("r"));
        assert!(matches!(
            idem.reserve(&kh("r"), &fp("args-b")),
            Reserved::Fresh
        ));
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
        idem.reserve(&kh("k0"), &fp("args"));
        idem.settle(&kh("k0"), Settled::Unknown("helper_exited".into()));
        for i in 1..MAX_KEYS {
            let key = kh(&format!("k{i}"));
            idem.reserve(&key, &fp("args"));
            idem.settle(&key, Settled::Done(json!({ "i": i })));
        }
        assert!(matches!(
            idem.reserve(&kh("k-live"), &fp("args")),
            Reserved::Fresh
        ));

        // Cap reached: the next reserve evicts the OLDEST Done (k1) — not
        // the older unknown pin (k0), never the in-flight key.
        assert!(matches!(
            idem.reserve(&kh("k-new"), &fp("args")),
            Reserved::Fresh
        ));
        match idem.reserve(&kh("k0"), &fp("args")) {
            Reserved::Unknown(code) => assert_eq!(code, "helper_exited"),
            other => panic!("unknown pin must outlive settled sends, got {other:?}"),
        }
        assert!(
            matches!(idem.reserve(&kh("k1"), &fp("args")), Reserved::Fresh),
            "the oldest completed outcome is the eviction victim"
        );
        assert!(
            matches!(idem.reserve(&kh("k-live"), &fp("args")), Reserved::InFlight),
            "in-flight keys are never evicted"
        );
        // The reserved-not-yet-settled key is still in flight; once its
        // owner settles, repeats replay.
        assert!(matches!(
            idem.reserve(&kh("k-new"), &fp("args")),
            Reserved::InFlight
        ));
        idem.settle(&kh("k-new"), Settled::Done(json!({ "n": 1 })));
        match idem.reserve(&kh("k-new"), &fp("args")) {
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

    // ---- durable ledger ----

    #[test]
    fn ledger_round_trip_replays_done_and_keeps_unknown_pins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);
        let now = unix_now();

        let mut first = Idempotency::default();
        first.reserve(&kh("done-key"), &fp("args-a"));
        first.settle(&kh("done-key"), Settled::Done(json!({ "sent": true })));
        first.reserve(&kh("pinned-key"), &fp("args-b"));
        first.settle(&kh("pinned-key"), Settled::Unknown("timeout".into()));
        first.persist_ledger(&path).unwrap();

        // A fresh process reloads: the completed outcome replays verbatim,
        // the unknown pin still refuses — the double-send guard survives
        // the restart (the exact gap the ledger closes).
        let mut reloaded = load_ledger(&path, now + 1);
        match reloaded.reserve(&kh("done-key"), &fp("args-a")) {
            Reserved::Done(v) => assert_eq!(v, json!({ "sent": true })),
            other => panic!("a settled send must replay after reload, got {other:?}"),
        }
        match reloaded.reserve(&kh("pinned-key"), &fp("args-b")) {
            Reserved::Unknown(code) => assert_eq!(code, "timeout"),
            other => panic!("an unknown pin must survive reload, got {other:?}"),
        }
        // The fingerprint contract survives too: different arguments on a
        // reloaded key are a reuse refusal, not a replay.
        assert!(matches!(
            reloaded.reserve(&kh("done-key"), &fp("args-z")),
            Reserved::Reused
        ));
    }

    #[test]
    fn ledger_drops_entries_past_the_ttl_and_keeps_fresh_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);
        let now = 1_000_000u64;
        let stale = now - PERSIST_TTL_SECS - 1;
        let fresh = now - PERSIST_TTL_SECS; // exactly at the boundary: kept
        let doc = json!({
            "v": 1,
            "entries": [
                { "k": hex(&kh("stale")), "f": hex(&fp("args")), "s": "unknown",
                  "o": null, "c": "timeout", "t": stale },
                { "k": hex(&kh("fresh")), "f": hex(&fp("args")), "s": "unknown",
                  "o": null, "c": "helper_exited", "t": fresh },
            ]
        });
        std::fs::write(&path, doc.to_string()).unwrap();

        let mut reloaded = load_ledger(&path, now);
        assert!(
            matches!(reloaded.reserve(&kh("stale"), &fp("args")), Reserved::Fresh),
            "an entry past the TTL is forgotten"
        );
        assert!(matches!(
            reloaded.reserve(&kh("fresh"), &fp("args")),
            Reserved::Unknown(_)
        ));
    }

    #[test]
    fn corrupt_or_absent_ledger_starts_unrecorded() {
        let dir = tempfile::tempdir().unwrap();
        // Absent: the first-run case.
        let mut missing = load_ledger(&dir.path().join(LEDGER_FILE), unix_now());
        assert!(matches!(
            missing.reserve(&kh("k"), &fp("args")),
            Reserved::Fresh
        ));
        // Corrupt: noted on stderr, never fatal — the server must serve.
        let path = dir.path().join(LEDGER_FILE);
        std::fs::write(&path, b"{not json").unwrap();
        let mut corrupt = load_ledger(&path, unix_now());
        assert!(matches!(
            corrupt.reserve(&kh("k"), &fp("args")),
            Reserved::Fresh
        ));
        // Malformed rows inside a valid document drop individually.
        let doc = json!({
            "v": 1,
            "entries": [
                { "k": "nothex", "f": "nothex", "s": "done", "o": {"sent": true}, "t": 1 },
                { "k": hex(&kh("ok")), "f": "short", "s": "done", "o": {"sent": true}, "t": 1 },
                { "k": hex(&kh("no-outcome")), "f": hex(&fp("args")), "s": "done", "o": null, "t": 1 },
                { "k": hex(&kh("no-code")), "f": hex(&fp("args")), "s": "unknown", "c": null, "t": 1 },
                { "k": hex(&kh("future-kind")), "f": hex(&fp("args")), "s": "queued", "o": {"x": 1}, "t": 1 },
            ]
        });
        std::fs::write(&path, doc.to_string()).unwrap();
        let mut partial = load_ledger(&path, unix_now());
        for key in ["nothex", "ok", "no-outcome", "no-code", "future-kind"] {
            assert!(
                matches!(partial.reserve(&kh(key), &fp("args")), Reserved::Fresh),
                "malformed row {key} must be dropped, not loaded"
            );
        }
    }

    #[test]
    fn ledger_never_contains_raw_keys_or_in_flight_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);

        let mut idem = Idempotency::default();
        idem.reserve(&kh("secret-client-key"), &fp("args"));
        idem.settle(
            &kh("secret-client-key"),
            Settled::Done(json!({ "sent": true })),
        );
        // An in-flight claim must not reach the file: a restart kills the
        // dispatching thread, and a stale in-flight row would refuse
        // retries of a call that provably never landed.
        idem.reserve(&kh("in-flight-only"), &fp("args"));
        idem.persist_ledger(&path).unwrap();

        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(
            !bytes.contains("secret-client-key") && !bytes.contains("in-flight-only"),
            "the ledger stores key hashes only — raw client keys must never hit disk"
        );
        assert!(
            bytes.contains(&hex(&kh("secret-client-key"))),
            "the settled key's hash is the stored form"
        );
        assert!(
            !bytes.contains(&hex(&kh("in-flight-only"))),
            "in-flight claims are never persisted"
        );
    }

    #[test]
    fn ledger_load_honors_the_key_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);
        let now = 5_000_000u64;
        let entries: Vec<Value> = (0..MAX_KEYS + 2)
            .map(|i| {
                json!({
                    "k": hex(&kh(&format!("cap-{i}"))),
                    "f": hex(&fp("args")),
                    "s": "unknown",
                    "o": null,
                    "c": "timeout",
                    "t": now - (MAX_KEYS as u64 + 2) + i as u64, // ascending: oldest first
                })
            })
            .collect();
        std::fs::write(&path, json!({ "v": 1, "entries": entries }).to_string()).unwrap();

        let mut reloaded = load_ledger(&path, now);
        // The OLDEST two are dropped; the newest MAX_KEYS survive — the
        // same bound memory enforces, applied at load so a hand-grown file
        // cannot enlarge the retained set. Survivors are probed FIRST: a
        // reserve on an unknown pin never mutates the table, while a Fresh
        // probe inserts an in-flight entry — at the cap that evicts the
        // oldest survivor, so probing the dropped keys first would evict
        // the very entries this test asserts survive.
        for i in 2..MAX_KEYS + 2 {
            assert!(
                matches!(
                    reloaded.reserve(&kh(&format!("cap-{i}")), &fp("args")),
                    Reserved::Unknown(_)
                ),
                "newest entry cap-{i} must survive the load cap"
            );
        }
        assert!(matches!(
            reloaded.reserve(&kh("cap-0"), &fp("args")),
            Reserved::Fresh
        ));
        assert!(matches!(
            reloaded.reserve(&kh("cap-1"), &fp("args")),
            Reserved::Fresh
        ));
    }

    #[test]
    fn key_hash_is_stable_and_discriminating() {
        assert_eq!(key_hash("k"), key_hash("k"));
        assert_ne!(key_hash("k"), key_hash("k "));
        assert_eq!(key_hash("k").len(), 32);
        // hex/unhex round-trip the stored form.
        let digest = key_hash("round-trip");
        assert_eq!(unhex(&hex(&digest)), Some(digest));
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex(&"0".repeat(63)), None);
    }
}
