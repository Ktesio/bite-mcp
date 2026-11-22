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
//! after [`PERSIST_TTL_SECS`] (at load, at reserve, and at persist — a
//! long-lived server sheds expired rows as it goes) and the on-disk set
//! honors the same [`MAX_KEYS`] cap as memory. In-flight claims are
//! durable too: a fresh claim persists its pin BEFORE dispatch, so a
//! server exit mid-send leaves a row the next start loads as an unknown
//! pin (code [`INTERRUPTED`]) — the retry is refused instead of
//! re-dispatched. Because several server processes may share one data
//! dir, every persist MERGES the on-disk ledger: rows this process does
//! not OWN follow the disk copy (which may be newer than the snapshot
//! folded into memory, or gone because its writer released the key),
//! and a memory-fresh reserve consults the disk first
//! ([`Idempotency::reserve_durable`]) — the residual race (two
//! processes claiming the same free key in the same instant) is
//! inherent without an OS-level lock and documented in
//! docs/protocol.md. A missing, corrupt, or unreadable ledger starts the
//! server unrecorded, with one stderr note for the latter two.

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
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
/// SHA-256 treatment the fingerprints already use, so the key TEXT never
/// reaches the disk. (A completed send's stored outcome is replayed
/// verbatim, so its row carries what that send returned — subject and
/// recipients — as PRIVACY.md documents; only the key is hashed away.)
pub(crate) type KeyHash = [u8; 32];

/// How long a settled ledger entry survives from its settle time — on disk
/// and after reload. Retries of an ambiguous send arrive within seconds; a
/// week bounds how long a replay/refusal answer can lag behind what the
/// client remembers without inviting stale-key surprises.
pub(crate) const PERSIST_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Failure code for a key whose dispatch died with the server process:
/// the durable in-flight row survived, and the attempt may or may not
/// have committed — exactly the unknown-pin definition.
pub(crate) const INTERRUPTED: &str = "interrupted";

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

#[derive(Clone, Debug)]
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
#[derive(Clone, Debug)]
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
    /// Keys this process RESERVED and then cleanly released: their durable
    /// in-flight row (written at claim time) is stale, so the next persist
    /// must drop it instead of re-merging it as some other process's pin.
    /// Cleared after every successful persist — membership only matters
    /// between the release and the write that removes the row.
    dropped: HashSet<KeyHash>,
    /// Keys whose current row THIS process produced (a fresh claim, or the
    /// settle that followed it): for these the in-memory row is
    /// authoritative and beats the disk on every write. Every other row in
    /// `map` was folded in from the ledger — loaded at startup or merged
    /// from a disk read — and stays UN-owned: the writing process remains
    /// its authority, so a persist follows the DISK row for it (which may
    /// be newer, or gone because that process released the key).
    owned: HashSet<KeyHash>,
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
        // A settled entry past the TTL is forgotten, not answered from: a
        // long-lived server otherwise keeps replaying (and re-persisting)
        // week-old records the reload path would have dropped. Expiry is
        // measured from the settle time; an in-flight claim never expires.
        if let Some(entry) = self.map.get(key) {
            if let Some(settled_at) = entry.settled_at {
                if unix_now().saturating_sub(settled_at) > PERSIST_TTL_SECS {
                    self.map.remove(key);
                    if let Some(pos) = self.order.iter().position(|k| k == key) {
                        self.order.remove(pos);
                    }
                }
            }
        }
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
        self.owned.insert(*key);
        Reserved::Fresh
    }

    /// Reserve with cross-process and crash semantics (the server's entry
    /// point — plain [`reserve`] is the in-memory fast path tests use).
    /// When the key is free in MEMORY the durable ledger is consulted
    /// first: a second server process sharing the data dir may already
    /// have recorded (or pinned) this key, and its row is folded into
    /// memory so the verdict reflects it. A genuinely fresh claim
    /// persists its in-flight pin BEFORE the caller dispatches, so a
    /// server exit mid-send leaves a row the next start loads as an
    /// unknown pin ([`INTERRUPTED`]) — the same-key retry is refused
    /// instead of re-dispatched. Persist errors are noted on stderr and
    /// swallowed: durability degrades to this process, the send proceeds.
    pub(crate) fn reserve_durable(
        &mut self,
        key: &KeyHash,
        fingerprint: &Fingerprint,
        path: &std::path::Path,
    ) -> Reserved {
        // A memory row this process does NOT own was folded in from the
        // disk (startup load or an earlier merge) and may be stale: the
        // writing server may have settled it since, or released it (row
        // gone). Refresh from the durable row first so the verdict below
        // reflects the NEWEST durable state, not the first snapshot this
        // process ever took of it — otherwise a key another server
        // completed stays refused as `interrupted` here for the whole TTL.
        if self.map.contains_key(key) && !self.owned.contains(key) {
            self.merge_disk_row(key, fingerprint, path);
        }
        match self.reserve(key, fingerprint) {
            Reserved::Fresh => {}
            other => return other,
        }
        if let Some(verdict) = self.merge_disk_row(key, fingerprint, path) {
            return verdict;
        }
        if let Err(error) = self.persist_ledger(path) {
            let _ = writeln!(
                std::io::stderr(),
                "bite-mcp: idempotency ledger write failed ({error}); \
                 the in-flight pin is memory-only for this process"
            );
        }
        Reserved::Fresh
    }

    /// Fold the durable row for `key`, if any, into memory and return the
    /// verdict it implies. `None`: no durable row — the key is genuinely
    /// free (a stale un-owned memory row it replaces is forgotten). The
    /// loader maps on-disk in-flight rows to unknown pins, so a LIVE
    /// other-process claim reads as an ambiguous refusal here: this
    /// process cannot tell live from dead, and refusing is the safe side.
    /// (Row freshness rides the merge in [`persist_ledger`], which follows
    /// the disk row for every key this process does not own.)
    fn merge_disk_row(
        &mut self,
        key: &KeyHash,
        fingerprint: &Fingerprint,
        path: &std::path::Path,
    ) -> Option<Reserved> {
        let disk = load_ledger(path, unix_now());
        let Some(entry) = disk.map.get(key).cloned() else {
            // No durable row. If memory still holds a row for this key
            // that this process does not own, the server that wrote it
            // has since released it (or the ledger was wiped): forget
            // the stale snapshot — the key is genuinely free. An OWNED
            // row here is a fresh claim whose pin has not been persisted
            // yet; it stays.
            if !self.owned.contains(key) {
                self.forget(key);
            }
            return None;
        };
        if entry.fingerprint != *fingerprint {
            // Another process recorded this key under different
            // arguments: keep the durable pin, refuse the reuse.
            self.adopt(key, entry);
            return Some(Reserved::Reused);
        }
        let verdict = match &entry.state {
            Recorded::Done(value) => Reserved::Done(value.clone()),
            Recorded::Unknown(code) => Reserved::Unknown(code.clone()),
            Recorded::InFlight => Reserved::InFlight,
        };
        self.adopt(key, entry);
        Some(verdict)
    }

    /// Install a durable row folded in from the disk. A row this process
    /// already holds (its own fresh `reserve` claim on the
    /// `reserve_durable` path, or the un-owned snapshot being refreshed)
    /// is REPLACED in place — the key enters `order` exactly once, keeping
    /// order/map in lockstep. Adopted rows are never `owned`: the writing
    /// process remains their authority.
    fn adopt(&mut self, key: &KeyHash, entry: Entry) {
        if !self.map.contains_key(key) {
            self.evict_if_full();
            self.order.push_back(*key);
        }
        self.owned.remove(key);
        self.map.insert(*key, entry);
    }

    /// Remove a key's row together with its `order` slot and ownership —
    /// used when a durable row this process folded in has vanished from
    /// the ledger (its writer released the key, or the ledger was wiped).
    fn forget(&mut self, key: &KeyHash) {
        self.map.remove(key);
        self.owned.remove(key);
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
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
    /// nothing for new arguments to desynchronize. The durable in-flight
    /// row written at claim time is now stale: the key is remembered in
    /// `dropped` so the next persist writes the ledger WITHOUT it (use
    /// [`release_and_persist`] at the call site so that write happens).
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
            self.owned.remove(key);
            self.dropped.insert(*key);
        }
    }

    /// Release and best-effort persist: without the write, the claim's
    /// durable in-flight row would survive this process and pin the key
    /// as interrupted at the next start — a key this process provably
    /// never sent under.
    pub(crate) fn release_and_persist(&mut self, key: &KeyHash, path: &std::path::Path) {
        self.release(key);
        if let Err(error) = self.persist_ledger(path) {
            let _ = writeln!(
                std::io::stderr(),
                "bite-mcp: idempotency ledger write failed ({error}); the \
                 released key stays pinned on disk until a later write"
            );
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

    /// Serialize the table into the durable ledger JSON and write it 0600
    /// via the shared private-write helper (temp file + rename, so a crash
    /// never leaves a half-written table). Four behaviors keep the file
    /// honest across processes and time:
    ///
    /// * settled entries past [`PERSIST_TTL_SECS`] are not written — the
    ///   file sheds dead rows on every write, not only at load;
    /// * in-flight claims ARE written (as `inflight` rows keyed by hash):
    ///   a process that dies mid-dispatch leaves the pin behind, and the
    ///   next start loads it as an unknown pin (`interrupted`) instead of
    ///   reopening the double-send window — a cleanly released claim's
    ///   row is dropped here via the `dropped` set;
    /// * rows in the ON-DISK ledger for keys this process does not OWN
    ///   are re-written verbatim — several servers may share one data
    ///   dir, and a persist must never erase another process's records.
    ///   Ownership matters: a row this process loaded or merged but never
    ///   claimed itself is the OTHER server's record, so the disk copy
    ///   (which may have settled since) wins — and when such a row has
    ///   VANISHED from disk its writer released the key, so the stale
    ///   memory snapshot is dropped instead of being written back;
    /// * the merged set is cut to [`MAX_KEYS`] before writing, with the
    ///   same victim rule as memory (oldest `done` first, then oldest
    ///   `unknown`; an in-flight pin is never a victim).
    ///
    /// The residual race (two processes writing in the same instant, or
    /// claiming the same free key concurrently) is inherent without an
    /// OS-level lock and documented in docs/protocol.md.
    pub(crate) fn persist_ledger(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        let now = unix_now();
        let expired = |settled_at: u64| now.saturating_sub(settled_at) > PERSIST_TTL_SECS;
        // (settle time, victim rank, record). Rank drives the on-disk cap:
        // 0 = done (first victim), 1 = unknown, 2 = in-flight (protected,
        // mirroring `evict_if_full`).
        let mut rows: Vec<(u64, u8, Value)> = self
            .order
            .iter()
            // Un-owned rows are re-derived from the disk below: this
            // process's snapshot of them may be older than the writer's.
            .filter(|key| self.owned.contains(*key))
            .filter_map(|key| self.map.get(key).map(|entry| (*key, entry)))
            .filter_map(|(key, entry)| {
                let record = match &entry.state {
                    Recorded::Done(value) => {
                        let settled_at = entry.settled_at?;
                        if expired(settled_at) {
                            return None;
                        }
                        (
                            settled_at,
                            0,
                            json_record(&key, entry, "done", Some(value), None, settled_at),
                        )
                    }
                    Recorded::Unknown(code) => {
                        let settled_at = entry.settled_at?;
                        if expired(settled_at) {
                            return None;
                        }
                        (
                            settled_at,
                            1,
                            json_record(&key, entry, "unknown", None, Some(code), settled_at),
                        )
                    }
                    // The claim time is the write time: the row expires
                    // from its last refresh, so a live process's pins stay
                    // current while an abandoned one ages out within the
                    // TTL.
                    Recorded::InFlight => (
                        now,
                        2,
                        serde_json::json!({
                            "k": hex(&key),
                            "f": hex(&entry.fingerprint),
                            "s": "inflight",
                            "t": now,
                        }),
                    ),
                };
                Some(record)
            })
            .collect();
        // Merge the on-disk ledger: rows this process does not own (and
        // has not deliberately released) are re-written verbatim.
        // `load_ledger` already dropped expired and malformed rows.
        let disk = load_ledger(path, now);
        for key in &disk.order {
            if self.owned.contains(key) || self.dropped.contains(key) {
                continue;
            }
            let Some(entry) = disk.map.get(key) else {
                continue;
            };
            let Some(settled_at) = entry.settled_at else {
                continue; // the loader never yields in-flight rows
            };
            let record = match &entry.state {
                Recorded::Done(value) => (
                    settled_at,
                    0,
                    json_record(key, entry, "done", Some(value), None, settled_at),
                ),
                Recorded::Unknown(code) => (
                    settled_at,
                    1,
                    json_record(key, entry, "unknown", None, Some(code), settled_at),
                ),
                Recorded::InFlight => continue,
            };
            rows.push(record);
        }
        // Un-owned memory rows whose durable row has vanished: the writer
        // released the key (or the ledger was wiped) — forget the stale
        // snapshot so it neither answers a later reserve nor rides back
        // to disk on a later write.
        let snapshot: Vec<KeyHash> = self.order.iter().copied().collect();
        for key in snapshot {
            if !self.owned.contains(&key)
                && !disk.map.contains_key(&key)
                && !self.dropped.contains(&key)
            {
                self.forget(&key);
            }
        }
        // The on-disk set honors the same cap as memory. Oldest first
        // (settle time; stable sort keeps insertion order on ties), a
        // done row evicted before an unknown pin, an in-flight pin never.
        rows.sort_by_key(|(settled_at, _, _)| *settled_at);
        while rows.len() > MAX_KEYS {
            let victim = rows
                .iter()
                .position(|(_, rank, _)| *rank == 0)
                .or_else(|| rows.iter().position(|(_, rank, _)| *rank == 1));
            let Some(pos) = victim else {
                break; // only protected in-flight rows remain
            };
            rows.remove(pos);
        }
        let entries: Vec<Value> = rows.into_iter().map(|(_, _, record)| record).collect();
        let doc = serde_json::json!({ "v": 1, "entries": entries });
        bite_core::fsops::write_private(path, doc.to_string().as_bytes())?;
        self.dropped.clear();
        Ok(())
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
/// the same cap memory enforces. An on-disk in-flight row (its process
/// died mid-dispatch, or another server sharing the data dir is still
/// running) loads as an unknown pin with code [`INTERRUPTED`]: the
/// attempt may or may not have committed, and refusing the retry is the
/// safe answer. A missing file is the first-run case (silent); a corrupt
/// or unreadable file is noted on stderr (the operator channel) and
/// treated the same: the server starts unrecorded rather than refusing to
/// serve, trading a lost replay/refusal surface for availability.
pub(crate) fn load_ledger(path: &std::path::Path, now_secs: u64) -> Idempotency {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        // Absent: a first run (or a purge) — not worth an operator line.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Idempotency::default()
        }
        // Present but unreadable (permissions, I/O error): same degraded
        // start, but the operator gets the one-line note the protocol
        // document promises.
        Err(error) => {
            let _ = writeln!(
                std::io::stderr(),
                "bite-mcp: idempotency ledger is unreadable ({error}); starting unrecorded"
            );
            return Idempotency::default();
        }
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
            // The writing process died (or is still running elsewhere)
            // with the claim open: the attempt may or may not have
            // committed — pin it as unknown rather than re-dispatch.
            Some("inflight") => Recorded::Unknown(INTERRUPTED.to_string()),
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
        let now = unix_now();
        // Real-clock timestamps: the reserve path now ALSO enforces the
        // TTL against the live clock, so epoch-scale synthetic times would
        // read as long-expired there and mask the load-side assertions.
        let stale = now.saturating_sub(PERSIST_TTL_SECS + 1);
        let fresh = now.saturating_sub(PERSIST_TTL_SECS); // boundary: kept
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
    fn ledger_stores_key_hashes_and_durable_in_flight_pins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);

        let mut idem = Idempotency::default();
        idem.reserve(&kh("secret-client-key"), &fp("args"));
        idem.settle(
            &kh("secret-client-key"),
            Settled::Done(json!({ "sent": true })),
        );
        // An in-flight claim DOES reach the file now (keyed by hash): the
        // claim must outlive a process that dies mid-dispatch, or a
        // same-key retry after the restart re-dispatches and double-sends.
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
            bytes.contains(&hex(&kh("in-flight-only"))),
            "an in-flight claim persists as a durable pin"
        );
        // The persisted pin loads as an UNKNOWN pin (interrupted): the
        // attempt may or may not have committed, so the retry is refused.
        let mut reloaded = load_ledger(&path, unix_now());
        match reloaded.reserve(&kh("in-flight-only"), &fp("args")) {
            Reserved::Unknown(code) => assert_eq!(code, INTERRUPTED),
            other => panic!("a crashed in-flight claim must pin unknown, got {other:?}"),
        }
    }

    #[test]
    fn released_claims_leave_no_durable_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);

        // A clean failure releases the claim AND unpersists the pin: a
        // later restart must see the key as free, not as interrupted.
        let mut idem = Idempotency::default();
        idem.reserve_durable(&kh("clean-fail"), &fp("args"), &path);
        idem.release_and_persist(&kh("clean-fail"), &path);

        let mut reloaded = load_ledger(&path, unix_now());
        assert!(
            matches!(
                reloaded.reserve(&kh("clean-fail"), &fp("args")),
                Reserved::Fresh
            ),
            "a cleanly released claim must not pin the key across restarts"
        );
    }

    #[test]
    fn persist_merges_rows_written_by_other_processes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);

        // Process A settles its key and writes the shared ledger.
        let mut a = Idempotency::default();
        a.reserve(&kh("a-key"), &fp("args-a"));
        a.settle(&kh("a-key"), Settled::Done(json!({ "sent": true })));
        a.persist_ledger(&path).unwrap();

        // Process B — same data dir, its own memory — settles a different
        // key and persists: A's row must survive B's write.
        let mut b = Idempotency::default();
        b.reserve(&kh("b-key"), &fp("args-b"));
        b.settle(&kh("b-key"), Settled::Done(json!({ "sent": true })));
        b.persist_ledger(&path).unwrap();

        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(
            bytes.contains(&hex(&kh("a-key"))) && bytes.contains(&hex(&kh("b-key"))),
            "a persist must merge, not overwrite: {bytes}"
        );
        // And a fresh reader sees both replays.
        let mut reloaded = load_ledger(&path, unix_now());
        assert!(matches!(
            reloaded.reserve(&kh("a-key"), &fp("args-a")),
            Reserved::Done(_)
        ));
        assert!(matches!(
            reloaded.reserve(&kh("b-key"), &fp("args-b")),
            Reserved::Done(_)
        ));
    }

    #[test]
    fn reserve_durable_keeps_order_and_map_in_lockstep_for_disk_backed_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);

        // Server A settles a key and writes the shared ledger.
        let mut a = Idempotency::default();
        a.reserve(&kh("disk-key"), &fp("args"));
        a.settle(&kh("disk-key"), Settled::Done(json!({ "sent": true })));
        a.persist_ledger(&path).unwrap();

        // Server B folds the row in via reserve_durable: a key that is
        // already in memory (the fresh claim `reserve` just made) must be
        // REPLACED, never given a second `order` slot.
        let mut b = load_ledger(&path, unix_now());
        assert!(matches!(
            b.reserve_durable(&kh("disk-key"), &fp("args"), &path),
            Reserved::Done(_)
        ));
        assert_eq!(
            b.order.len(),
            b.map.len(),
            "a disk-backed key must enter order exactly once"
        );
        assert_eq!(b.order.iter().filter(|k| **k == kh("disk-key")).count(), 1);

        // And after memory evicts the key (table fills past the cap) it
        // lives on disk only; reserving it again must still not duplicate
        // the slot — dead slots would eat the cap.
        for i in 0..MAX_KEYS {
            let key = kh(&format!("fill-{i}"));
            b.reserve(&key, &fp("args"));
            b.settle(&key, Settled::Done(json!({ "sent": true })));
        }
        b.reserve_durable(&kh("disk-key"), &fp("args"), &path);
        assert_eq!(
            b.order.iter().filter(|k| **k == kh("disk-key")).count(),
            1,
            "no duplicate order slots for a disk-only key"
        );
        assert_eq!(b.order.len(), b.map.len());
    }

    #[test]
    fn persist_follows_newer_disk_rows_for_unowned_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);

        // B claims the shared key; its in-flight pin hits the disk.
        let mut b = Idempotency::default();
        assert!(matches!(
            b.reserve(&kh("shared"), &fp("args")),
            Reserved::Fresh
        ));
        b.persist_ledger(&path).unwrap();

        // A starts on the same data dir: the row loads as an unknown pin
        // (interrupted) — a snapshot A does not own.
        let mut a = load_ledger(&path, unix_now());
        assert!(matches!(
            a.reserve(&kh("shared"), &fp("args")),
            Reserved::Unknown(_)
        ));

        // B finishes: the disk now says done.
        b.settle(&kh("shared"), Settled::Done(json!({ "sent": true })));
        b.persist_ledger(&path).unwrap();

        // A settles its OWN key and persists: the shared key is not A's
        // to answer — B's completed row must survive, and A's stale
        // interrupted snapshot must not be written over it.
        assert!(matches!(
            a.reserve(&kh("own"), &fp("args")),
            Reserved::Fresh
        ));
        a.settle(&kh("own"), Settled::Done(json!({ "sent": true })));
        a.persist_ledger(&path).unwrap();

        let mut reloaded = load_ledger(&path, unix_now());
        match reloaded.reserve(&kh("shared"), &fp("args")) {
            Reserved::Done(v) => assert_eq!(v, json!({ "sent": true })),
            other => panic!("the other server's completed send must survive, got {other:?}"),
        }

        // The reserve path refreshes too: even A's stale in-memory
        // snapshot gives way to the durable done row on the next ask.
        match a.reserve_durable(&kh("shared"), &fp("args"), &path) {
            Reserved::Done(v) => assert_eq!(v, json!({ "sent": true })),
            other => {
                panic!("a stale un-owned snapshot must not outlive the disk row, got {other:?}")
            }
        }
    }

    #[test]
    fn vanished_unowned_rows_release_the_key_for_this_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);

        // B claims and persists the in-flight pin; A loads it as an
        // interrupted pin.
        let mut b = Idempotency::default();
        assert!(matches!(
            b.reserve(&kh("shared"), &fp("args")),
            Reserved::Fresh
        ));
        b.persist_ledger(&path).unwrap();
        let mut a = load_ledger(&path, unix_now());
        assert!(matches!(
            a.reserve(&kh("shared"), &fp("args")),
            Reserved::Unknown(_)
        ));

        // B's attempt fails cleanly: released, row gone from the ledger.
        b.release_and_persist(&kh("shared"), &path);

        // A persists for its own key: the vanished row must leave A's
        // memory instead of riding back to disk.
        assert!(matches!(
            a.reserve(&kh("own"), &fp("args")),
            Reserved::Fresh
        ));
        a.settle(&kh("own"), Settled::Done(json!({ "sent": true })));
        a.persist_ledger(&path).unwrap();
        assert!(
            !a.order.contains(&kh("shared")),
            "a key its writer released must not ride back to disk"
        );
        assert_eq!(a.order.len(), a.map.len());

        // And A's next reserve of that key claims it fresh — the writer
        // provably never sent under it.
        assert!(
            matches!(
                a.reserve_durable(&kh("shared"), &fp("args"), &path),
                Reserved::Fresh
            ),
            "a released-by-writer key must free up for this server"
        );
    }

    #[test]
    fn persist_caps_the_merged_disk_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);

        // A foreign ledger already at the cap, written with older settle
        // times so the victim choice below is deterministic.
        let now = unix_now();
        let old: Vec<Value> = (0..MAX_KEYS)
            .map(|i| {
                serde_json::json!({
                    "k": hex(&kh(&format!("old-{i}"))),
                    "f": hex(&fp("args")),
                    "s": "done",
                    "o": { "sent": true },
                    "t": now - 100,
                })
            })
            .collect();
        std::fs::write(
            &path,
            serde_json::json!({ "v": 1, "entries": old }).to_string(),
        )
        .unwrap();

        // This server adds five OWN keys (settled now — strictly newer)
        // and persists: the merged set must still cap at MAX_KEYS, with
        // the OLDEST foreign rows as the victims.
        let mut a = load_ledger(&path, unix_now());
        for i in 0..5 {
            let key = kh(&format!("new-{i}"));
            assert!(matches!(a.reserve(&key, &fp("args")), Reserved::Fresh));
            a.settle(&key, Settled::Done(json!({ "sent": true })));
        }
        a.persist_ledger(&path).unwrap();

        let mut reloaded = load_ledger(&path, unix_now());
        assert_eq!(
            reloaded.map.len(),
            MAX_KEYS,
            "the on-disk cap must hold after a merge"
        );
        for i in 0..5 {
            assert!(
                matches!(
                    reloaded.reserve(&kh(&format!("new-{i}")), &fp("args")),
                    Reserved::Done(_)
                ),
                "the newest rows must never be the victims"
            );
        }
    }

    #[test]
    fn reserve_durable_folds_disk_rows_into_a_fresh_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);

        // A settles and exits; a second server sharing the data dir must
        // NOT treat the key as free just because its own memory is empty.
        let mut a = Idempotency::default();
        a.reserve(&kh("cross-process"), &fp("args"));
        a.settle(&kh("cross-process"), Settled::Done(json!({ "sent": true })));
        a.persist_ledger(&path).unwrap();

        let mut b = Idempotency::default();
        match b.reserve_durable(&kh("cross-process"), &fp("args"), &path) {
            Reserved::Done(v) => assert_eq!(v, json!({ "sent": true })),
            other => panic!("the disk row must win over empty memory, got {other:?}"),
        }
        // Different arguments against the durable row: refused reuse.
        assert!(matches!(
            b.reserve_durable(&kh("cross-process"), &fp("other"), &path),
            Reserved::Reused
        ));
        // A genuinely unknown key still claims fresh (and pins durably).
        assert!(matches!(
            b.reserve_durable(&kh("unrelated"), &fp("args"), &path),
            Reserved::Fresh
        ));
        let mut reloaded = load_ledger(&path, unix_now());
        match reloaded.reserve(&kh("unrelated"), &fp("args")) {
            Reserved::Unknown(code) => assert_eq!(code, INTERRUPTED),
            other => panic!("a fresh durable claim must leave a pin, got {other:?}"),
        }
    }

    #[test]
    fn long_lived_server_expires_settled_entries_at_reserve_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);

        let mut idem = Idempotency::default();
        idem.reserve(&kh("aged"), &fp("args"));
        idem.settle(&kh("aged"), Settled::Done(json!({ "sent": true })));
        idem.persist_ledger(&path).unwrap();
        // Backdate the settle time past the TTL, as a server that has
        // simply been up for over a week would have in memory.
        if let Some(entry) = idem.map.get_mut(&kh("aged")) {
            entry.settled_at = Some(1); // epoch: expired for any real clock
        }
        // …and persist sheds the dead row instead of rewriting it: the
        // entry is still IN memory (expired), and the write skips it.
        idem.persist_ledger(&path).unwrap();
        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(
            !bytes.contains(&hex(&kh("aged"))),
            "an expired row must not be re-persisted"
        );
        // Reserve forgets the aged entry instead of replaying it…
        assert!(
            matches!(idem.reserve(&kh("aged"), &fp("args")), Reserved::Fresh),
            "a settled entry past the TTL must not be answered from memory"
        );
    }

    #[test]
    fn ledger_load_honors_the_key_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);
        let now = unix_now(); // real clock: reserve also enforces the TTL
        let entries: Vec<Value> = (0..MAX_KEYS + 2)
            .map(|i| {
                json!({
                    "k": hex(&kh(&format!("cap-{i}"))),
                    "f": hex(&fp("args")),
                    "s": "unknown",
                    "o": null,
                    "c": "timeout",
                    // ascending: oldest first (all within the TTL)
                    "t": now - (MAX_KEYS as u64 + 2) + i as u64,
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
