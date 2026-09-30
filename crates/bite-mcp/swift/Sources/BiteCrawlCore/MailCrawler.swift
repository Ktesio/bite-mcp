// Mail crawler core: window scheduling (30d initial + 10d backfill),
// chunked OSA reads, JSONL batch staging.
//
// Runs inside the standalone `bite-crawl` process. Lifetime is fully
// detached from the helper/CLI/MCP: progress is published through a
// callback + a crawl-state.json file the Rust control plane polls.
//
// Mail transport: every read goes through MailAE's serialized NSAppleScript
// executor (raw Apple Event sending is broken on macOS 27). Message reads
// fetch one Apple Event per chunk — full `properties` records when bodies
// are wanted, an explicit 7-property bundle when they are not — bounded
// well under Mail's 120 s AppleEvent timeout.

import Foundation

public struct CrawlWindow {
    public let fromMs: Int64
    public let toMs: Int64

    public init(fromMs: Int64, toMs: Int64) {
        self.fromMs = fromMs
        self.toMs = toMs
    }
}

/// JSONL batch record — mirrors bite-index::store::Record (Rust).
public struct CrawlRecord: Codable {
    public let app: String
    public let id: String
    public var account: String?
    public var container: String?
    public var title: String?
    public var content: String?
    public var participants: String?
    public var start_ms: Int64?
    public var end_ms: Int64?
    public var updated_ms: Int64?
    public var read: Bool?
    public var flagged: Bool?
    public var junk: Bool?
    public var completed: Bool?
    public var priority: Int?
    public var props: String?

    public init(app: String, id: String, account: String?, container: String?,
                title: String?, content: String?, participants: String?,
                start_ms: Int64?, end_ms: Int64?, updated_ms: Int64?,
                read: Bool?, flagged: Bool?, junk: Bool?, completed: Bool?,
                priority: Int?, props: String?) {
        self.app = app; self.id = id; self.account = account; self.container = container
        self.title = title; self.content = content; self.participants = participants
        self.start_ms = start_ms; self.end_ms = end_ms; self.updated_ms = updated_ms
        self.read = read; self.flagged = flagged; self.junk = junk
        self.completed = completed; self.priority = priority; self.props = props
    }
}

/// Locked box for the not-yet-flushed batch records, so the SIGTERM handler
/// can flush a partial batch without racing the crawl thread. A failed
/// write re-appends the drained records — a transient batch-write failure
/// must not vaporize up to 50 records.
final class PendingBatch {
    private let lock = NSLock()
    private var records: [CrawlRecord] = []

    func append(_ r: CrawlRecord) {
        lock.lock(); records.append(r); lock.unlock()
    }

    func count() -> Int {
        lock.lock(); defer { lock.unlock() }
        return records.count
    }

    func drain() -> [CrawlRecord] {
        lock.lock(); defer { lock.unlock() }
        let out = records
        records = []
        return out
    }

    func refill(_ recs: [CrawlRecord]) {
        lock.lock(); records.append(contentsOf: recs); lock.unlock()
    }
}

public final class CrawlState {
    public static let shared = CrawlState()
    private let lock = NSLock()
    private var cancelled = false
    private var processed = 0
    private var found = 0
    private var window: String?
    private var flushPending: (() -> Bool)?
    private var jobStarted = false
    private var walkActive = false
    private var skippedMailboxes = 0
    private let cancelSem = DispatchSemaphore(value: 0)

    public init() {}

    public func stop() {
        lock.lock(); defer { lock.unlock() }
        cancelled = true
    }

    public var isCancelled: Bool {
        lock.lock(); defer { lock.unlock() }
        return cancelled
    }

    public func tally(processedAdd: Int, foundAdd: Int) {
        lock.lock(); defer { lock.unlock() }
        processed += processedAdd
        found += foundAdd
    }

    public var snapshot: (processed: Int, found: Int) {
        lock.lock(); defer { lock.unlock() }
        return (processed, found)
    }

    /// Last activity label written into crawl-state's `window` field. The
    /// worker's progress callback reads this so coarse progress reports can
    /// never clobber the wait-loop's attempt/diagnostic detail.
    public func setWindow(_ w: String?) {
        lock.lock(); defer { lock.unlock() }
        window = w
    }

    public var currentWindow: String? {
        lock.lock(); defer { lock.unlock() }
        return window
    }

    /// The walk's partial-batch flusher; invoked by the SIGTERM handler so
    /// a cancel never strands up to 50 staged records. Setting a flusher
    /// also marks the walk active and arms the cancel-observed semaphore.
    public func setFlushPending(_ f: (() -> Bool)?) {
        lock.lock()
        flushPending = f
        walkActive = (f != nil)
        while cancelSem.wait(timeout: .now()) == .success {}  // drain stale signals
        lock.unlock()
    }

    public func runFlushPending() -> Bool {
        lock.lock(); let f = flushPending; lock.unlock()
        return f?() ?? true
    }

    /// Per-run skip counter — the SIGTERM handler reads this so its
    /// cancelled write preserves the run's skip total.
    public func addSkippedMailbox() {
        lock.lock(); defer { lock.unlock() }
        skippedMailboxes += 1
    }

    public var skippedSoFar: Int {
        lock.lock(); defer { lock.unlock() }
        return skippedMailboxes
    }

    /// True once a real job has written a "running" state — the SIGTERM
    /// handler must not create a phantom cancelled state file on a
    /// never-crawled install (a phantom suppresses auto-respawn).
    public var hasStarted: Bool {
        lock.lock(); defer { lock.unlock() }
        return jobStarted
    }

    func markStarted() {
        lock.lock(); defer { lock.unlock() }
        jobStarted = true
    }

    public var isWalkActive: Bool {
        lock.lock(); defer { lock.unlock() }
        return walkActive
    }

    /// Signalled by the walk when it actually observes cancellation, so the
    /// SIGTERM handler can briefly wait for in-flight loop work to stop
    /// before its final flush.
    public func signalCancelledObserved() {
        cancelSem.signal()
    }

    public func waitCancelledObserved(timeout: TimeInterval) -> Bool {
        cancelSem.wait(timeout: .now() + timeout) == .success
    }
}

public enum MailCrawler {
    /// bite data dir root (batches/ and crawl-state.json live here).
    public static func dataDir() -> URL {
        let url = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("bite", isDirectory: true)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    public static func stagingDir() -> URL {
        let url = dataDir().appendingPathComponent("batches", isDirectory: true)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    public static func statePath() -> URL {
        dataDir().appendingPathComponent("crawl-state.json")
    }

    /// Methods the index layer implements (conformance-checked by CI).
    public static let supportedIndexMethods = [
        "index.crawl", "index.crawl_cancel", "index.crawl_status",
        "index.bulk_mark", "index.bulk_move", "index.bulk_delete", "index.search",
    ]

    static func isTerminalState(_ state: String) -> Bool {
        state == "done" || state == "failed" || state == "cancelled"
    }

    /// Failure counter carried in crawl-state ("failures", additive
    /// contract — Rust tolerates it missing). Reads the CURRENT file so a
    /// failed job can persist existing+1 and a successful one can zero it.
    public static func existingFailureCount() -> Int {
        guard let data = try? Data(contentsOf: statePath()),
              let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { return 0 }
        return obj["failures"] as? Int ?? 0
    }

    /// Serializes state-file writes: the SIGTERM handler and the crawl
    /// thread both call writeState, and unsynchronized remove+move could
    /// once leave NO state file behind.
    private static let stateLock = NSLock()

    /// Atomic replace: tmp → destination whether or not the destination
    /// already exists. Caller owns any serialization.
    static func atomicReplace(tmp: URL, dst: URL) -> Bool {
        let fm = FileManager.default
        do {
            if fm.fileExists(atPath: dst.path) {
                _ = try fm.replaceItemAt(dst, withItemAt: tmp)
            } else {
                try fm.moveItem(at: tmp, to: dst)
            }
            return true
        } catch {
            lastError = "atomic replace \(dst.lastPathComponent): \(error.localizedDescription)"
            return false
        }
    }

    /// Batch files are born-0600 (createFile with attributes BEFORE content
    /// lands), tmp names are unique, and the destination is replaced
    /// atomically. Returns false (with lastError) instead of swallowing
    /// failures — a lost batch must not look like a successful crawl.
    @discardableResult
    public static func writeBatch(staging: URL, jobID: String, seq: Int, records: [CrawlRecord]) -> Bool {
        var lines = ""
        for r in records {
            if let data = try? JSONEncoder().encode(r), let line = String(data: data, encoding: .utf8) {
                lines += line + "\n"
            }
        }
        guard let data = lines.data(using: .utf8) else {
            lastError = "batch encode failed"
            return false
        }
        let path = staging.appendingPathComponent("\(jobID)-\(seq).jsonl")
        let tmp = staging.appendingPathComponent("\(jobID)-\(seq).\(ProcessInfo.processInfo.globallyUniqueString).tmp")
        guard FileManager.default.createFile(atPath: tmp.path, contents: data,
                                             attributes: [.posixPermissions: 0o600]) else {
            lastError = "cannot create batch tmp \(tmp.path)"
            return false
        }
        return atomicReplace(tmp: tmp, dst: path)
    }

    /// Skipped-mailbox counter carried in crawl-state ("skipped",
    /// additive contract — Rust tolerates it missing; for operators).
    public static func existingSkippedCount() -> Int {
        guard let data = try? Data(contentsOf: statePath()),
              let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { return 0 }
        return obj["skipped"] as? Int ?? 0
    }

    public static func writeState(jobID: String, state: String, processed: Int, found: Int,
                                  window: String?, failures: Int? = nil, skipped: Int? = nil) {
        // once cancellation is armed ONLY the terminal cancelled state may
        // be written: heartbeats must not clobber it, and a post-cancel
        // failure write must not undo a user cancel (auto-respawn treats
        // failed as retry-now, which would override user intent)
        if CrawlState.shared.isCancelled && state != "cancelled" { return }
        if state == "running" { CrawlState.shared.markStarted() }
        CrawlState.shared.setWindow(window)
        var dict: [String: Any] = [
            "job_id": jobID,
            "state": state,
            "processed": processed,
            "found": found,
            "window": window ?? NSNull(),
            "updated_at": ISO8601DateFormatter().string(from: Date()),
        ]
        // counters: explicit override at terminal writes
        // (failed → existing+1, done → 0); everything else preserves
        dict["failures"] = failures ?? existingFailureCount()
        dict["skipped"] = skipped ?? existingSkippedCount()
        guard let data = try? JSONSerialization.data(withJSONObject: dict, options: [.prettyPrinted]) else { return }
        stateLock.lock()
        defer { stateLock.unlock() }
        // re-check under the lock: cancellation may have been armed (and
        // its terminal state written) while this call waited on the lock
        if CrawlState.shared.isCancelled && state != "cancelled" { return }
        let path = statePath()
        let tmp = path.appendingPathExtension("\(ProcessInfo.processInfo.globallyUniqueString).tmp")
        guard FileManager.default.createFile(atPath: tmp.path, contents: data,
                                             attributes: [.posixPermissions: 0o600]) else { return }
        _ = atomicReplace(tmp: tmp, dst: path)
    }

    /// Remove stale *.tmp droppings from a previous crashed run — but only
    /// ones older than 10 minutes, so a concurrent worker's live tmp file
    /// is never deleted underneath it.
    public static func sweepStaleTmp(at staging: URL) {
        let fm = FileManager.default
        guard let items = try? fm.contentsOfDirectory(at: staging, includingPropertiesForKeys: nil) else { return }
        for f in items where f.lastPathComponent.hasSuffix(".tmp") {
            if let attrs = try? fm.attributesOfItem(atPath: f.path),
               let mtime = attrs[.modificationDate] as? Date,
               Date().timeIntervalSince(mtime) < 600 {
                continue
            }
            try? fm.removeItem(at: f)
        }
    }

    public static func runJob(jobID: String, windowDays: Int, storeBody: Bool, mailboxFilter: String?,
                              seq: SeqCounter, progress: @escaping (String, Int, Int) -> Void) -> String {
        let state = CrawlState.shared
        guard MailAE.mailRunning() else {
            writeState(jobID: jobID, state: "failed", processed: 0, found: 0,
                       window: "Mail is not running", failures: existingFailureCount() + 1)
            progress("failed", 0, 0)
            return "failed"
        }

        let now = Int64(Date().timeIntervalSince1970 * 1000)
        let day: Int64 = 86_400_000
        var windows: [CrawlWindow] = [CrawlWindow(fromMs: now - 30 * day, toMs: now)]
        var edge = now - 30 * day
        while edge > now - 365 * day {
            windows.append(CrawlWindow(fromMs: max(edge - 10 * day, now - 365 * day), toMs: edge))
            edge -= 10 * day
        }

        // Mail's AE layer can stay saturated for many hours; keep retrying
        // for ~24 h before giving up (the control plane auto-respawns later
        // if we ever do exit). Heartbeat the state file every attempt.
        var accounts: [String]? = nil
        let maxWaitAttempts = 1440  // 24 h at 60 s intervals
        for attempt in 0..<maxWaitAttempts {
            if CrawlState.shared.isCancelled { break }
            if let list = MailAE.accountList() {
                accounts = list
                break
            }
            let diag = lastError.map { " — \($0)" } ?? ""
            writeState(jobID: jobID, state: "waiting_mail", processed: 0, found: 0,
                       window: "attempt \(attempt + 1)/\(maxWaitAttempts)\(diag)")
            progress("waiting_mail", 0, 0)
            Thread.sleep(forTimeInterval: 60)
        }
        guard let accounts else {
            writeState(jobID: jobID, state: "failed", processed: 0, found: 0,
                       window: "Mail unresponsive for 24 h", failures: existingFailureCount() + 1)
            progress("failed", 0, 0)
            return "failed"
        }

        var targets: [(account: String, index: Int, name: String)] = []
        var anyListSucceeded = false
        for acct in accounts {
            guard let boxes = MailAE.mailboxList(account: acct) else { continue }
            anyListSucceeded = true
            for (idx, boxName) in boxes.enumerated() {
                if let filter = mailboxFilter,
                   boxName.caseInsensitiveCompare(filter) != .orderedSame { continue }
                targets.append((account: acct, index: idx + 1, name: boxName))
            }
        }
        guard anyListSucceeded else {
            let diag = lastError ?? "unknown error"
            writeState(jobID: jobID, state: "failed", processed: 0, found: 0,
                       window: "cannot list mailboxes — \(diag)", failures: existingFailureCount() + 1)
            progress("failed", 0, 0)
            return "failed"
        }

        let staging = stagingDir()
        sweepStaleTmp(at: staging)

        var processed = 0
        var found = 0
        var skippedMailboxes = 0
        // Mailboxes whose counts have timed out: later windows skip the
        // count attempts and go straight to blind pagination (memoized per
        // job — 2×360 s of failed counting per window adds up to hours).
        var blindMailboxes = Set<String>()
        let windowLabel = { (w: CrawlWindow) -> String in "\(w.fromMs)-\(w.toMs)" }

        for window in windows {
            for t in targets {
                if state.isCancelled { break }
                let label = windowLabel(window)
                writeState(jobID: jobID, state: "running", processed: processed, found: found, window: label)
                progress("running", processed, found)
                let r = crawlMailbox(jobID: jobID, account: t.account, mailboxAt: t.index, mailbox: t.name,
                                     window: window, includeContent: storeBody,
                                     staging: staging, seq: seq, blindMailboxes: &blindMailboxes,
                                     progress: progress)
                if let err = r.transportError {
                    // a transport failure is a failed JOB, not a skipped
                    // mailbox — silently walking on would report done with
                    // holes in the index
                    let counters = terminalCounters(state: "failed",
                                                    previousFailures: existingFailureCount(),
                                                    previousSkipped: existingSkippedCount(),
                                                    newSkips: skippedMailboxes)
                    writeState(jobID: jobID, state: "failed",
                               processed: processed + r.scanned, found: found + r.indexed,
                               window: err, failures: counters.failures, skipped: counters.skipped)
                    progress("failed", processed + r.scanned, found + r.indexed)
                    return "failed"
                }
                processed += r.scanned
                found += r.indexed
                CrawlState.shared.tally(processedAdd: r.scanned, foundAdd: r.indexed)
                if let skip = r.skipReason {
                    // per-mailbox SKIP, not a job failure: the diagnostic
                    // lands in the state window label and the walk moves on
                    skippedMailboxes += 1
                    CrawlState.shared.addSkippedMailbox()
                    writeState(jobID: jobID, state: "running", processed: processed, found: found, window: skip)
                    progress("running", processed, found)
                    continue
                }
                progress("running", processed, found)
            }
            if state.isCancelled { break }
        }

        let finalState = state.isCancelled ? "cancelled" : "done"
        let counters = terminalCounters(state: finalState,
                                        previousFailures: existingFailureCount(),
                                        previousSkipped: existingSkippedCount(),
                                        newSkips: skippedMailboxes)
        writeState(jobID: jobID, state: finalState, processed: processed, found: found, window: nil,
                   failures: counters.failures, skipped: counters.skipped)
        progress(finalState, processed, found)
        return finalState
    }

    /// Walk one mailbox for one date window in index-range chunks. Order
    /// detected from dated samples (resampled against the far end when the
    /// first pair is ambiguous); the walk stops at the first confirmed
    /// window edge. A failed chunk retries the SAME range — transient
    /// errors never silently drop messages. Any abort (strikes, zero yield,
    /// identity drift, write failure) surfaces as transportError so the
    /// JOB fails instead of reporting done with holes.
    /// One mailbox-window walk result. `transportError` fails the whole
    /// job; `skipReason` (zero yield / order undetectable) abandons just
    /// this mailbox's window with a diagnostic while the job continues.
    struct WalkOutcome {
        var scanned: Int
        var indexed: Int
        var transportError: String?
        var skipReason: String?
    }

    /// Breaker: when a window accumulates mostly-useless rows (unreadable,
    /// or out-of-window skips) while indexing nothing, further chunks are
    /// wasted reads. The mailbox window is SKIPPED (diagnostic in the state
    /// label) — other mailboxes continue, and the job must not fail (a
    /// failure would loop-respawn against unparseable data). Only FAR-SIDE
    /// out-of-window skips feed this in descending walks; ascending walks
    /// feed all out-of-window skips (their pre-window approach prefix is
    /// the pathological burn case for blind-memoized old mailboxes).
    public static func zeroYieldSkipReason(unreadable: Int, outOfWindow: Int, indexed: Int, threshold: Int = 500) -> String? {
        let useless = unreadable + outOfWindow
        guard useless > threshold, indexed == 0 else { return nil }
        return "skipped: zero yield after \(useless) rows (unreadable \(unreadable), out-of-window \(outOfWindow))"
    }

    /// Order resolution with far-end resample. Returns the resolved
    /// direction plus whether it had to DEFAULT (both samples ambiguous) —
    /// a defaulted order with zero indexed rows later triggers the
    /// order-undetectable skip, so the flag must reflect the FINAL state:
    /// a successful far-end resample clears it (the stale-true variant
    /// falsely skipped resolvable mailboxes).
    public static func resolveOrder(firstSample: Date?, secondSample: Date?, farEndSample: Date?)
        -> (newestFirst: Bool, defaulted: Bool) {
        var order = MailAE.orderFromSamples(firstSample, secondSample)
        if order == nil { order = MailAE.orderFromSamples(firstSample, farEndSample) }
        return (order ?? true, order == nil)
    }

    /// Count failures: retry once with backoff, then fall back to blind
    /// pagination for that mailbox-window — AE counts on large mailboxes
    /// are O(n) and can exceed any timeout, while range reads on the same
    /// mailbox work. A slow count must never fail the job.
    public enum CountAttemptDecision: Equatable {
        case counted(total: Int)
        case retryAfterBackoff
        case enterBlindWalk
    }

    public static func countAttemptDecision(tryNumber: Int, maxAttempts: Int = 2, total: Int?) -> CountAttemptDecision {
        if let t = total { return .counted(total: t) }
        return tryNumber >= maxAttempts ? .enterBlindWalk : .retryAfterBackoff
    }

    /// Blind pagination: a chunk returning fewer rows than requested means
    /// the mailbox is exhausted — process what came back and stop cleanly.
    public static func isShortRead(rows: Int, requested: Int, blind: Bool) -> Bool {
        blind && rows < requested
    }

    /// Result of scanning one chunk: the in-window records to collect (in
    /// processing order), plus honest counters.
    public struct ChunkScan {
        public var records: [CrawlRecord]
        public var scanned: Int
        public var skipped: Int
        public var unreadable: Int
        public var skippedOutOfWindow: Int
        public var edgeCrossed: Bool
        public var orderMisDetected: Bool
    }

    /// Pure per-chunk core of the walk: maps one chunk's rows through the
    /// record mapping and the direction-aware window decision. `reverseRows`
    /// reverses within-chunk iteration (only counted oldest-first plans,
    /// which are laid out far-end-first); `edgeAscending` selects the edge
    /// polarity (only blind oldest-first walks end above toMs — everything
    /// else ends below fromMs). Splitting these two roles was the round-2
    /// critical fix: one combined flag mis-poled counted newest-first
    /// backfill windows (0 rows indexed per window).
    public static func scanChunk(_ rows: [NSAppleEventDescriptor?], window: CrawlWindow,
                                 reverseRows: Bool, edgeAscending: Bool,
                                 account: String, mailbox: String, includeContent: Bool,
                                 edgeTolerance: Int = 2, orderMisDetectTail: Int = 36,
                                 hasCollected: Bool = false) -> ChunkScan {
        var scan = ChunkScan(records: [], scanned: 0, skipped: 0, unreadable: 0,
                             skippedOutOfWindow: 0, edgeCrossed: false, orderMisDetected: false)
        var stale = 0
        var aboveWindowTail = 0
        var seenCollect = hasCollected  // walk-level: tripwire only arms after rows were indexed
        for row in reverseRows ? rows.reversed() : rows {
            scan.scanned += 1
            guard let record = MailAE.recordFromRow(row, account: account, mailbox: mailbox,
                                                    includeContent: includeContent),
                  let ms = record.start_ms else {
                scan.skipped += 1
                scan.unreadable += 1
                continue
            }
            let (decision, newStale) = rowDecision(ms: ms, fromMs: window.fromMs, toMs: window.toMs,
                                                   edgeAscending: edgeAscending,
                                                   staleSoFar: stale, tolerance: edgeTolerance)
            stale = newStale
            // FAR-SIDE skips feed the zero-yield breaker. The rule is
            // deliberately asymmetric: DESCENDING walks exclude their
            // approach side (above toMs — previous windows' volume; a large
            // prefix is NORMAL and must not false-trip the breaker), while
            // ASCENDING walks feed ALL out-of-window skips — their
            // pre-window approach prefix is exactly the pathological burn
            // (a blind-memoized Archive/Sent entirely older than the
            // window) that the breaker exists to stop.
            let farSide = edgeAscending ? true : (ms < window.fromMs)
            switch decision {
            case .edge:
                scan.edgeCrossed = true
                return scan
            case .skip:
                scan.skipped += 1
                if farSide {
                    scan.skippedOutOfWindow += 1
                } else if !edgeAscending, seenCollect, ms >= window.toMs {
                    // DESCENDING walks with a defaulted order: a long run of
                    // above-toMs rows AFTER rows were collected means the
                    // order was mis-detected — flag it (the caller skips the
                    // window; the job continues)
                    aboveWindowTail += 1
                    if aboveWindowTail > orderMisDetectTail {
                        scan.orderMisDetected = true
                    }
                }
                continue
            case .collect:
                seenCollect = true
                aboveWindowTail = 0
                scan.records.append(record)
            }
        }
        return scan
    }

    /// The two independent walk-direction roles for a mailbox-window walk —
    /// shared by production and the scanChunk table test so they cannot
    /// diverge:
    /// - reverseRows: reverse within-chunk iteration (only counted
    ///   oldest-first plans are laid out far-end-first)
    /// - edgeAscending: which window bound ends the walk (only blind
    ///   oldest-first walks end above toMs)
    public static func flagsForWalk(blind: Bool, newestFirstResolved: Bool) -> (reverseRows: Bool, edgeAscending: Bool) {
        (reverseRows: !blind && !newestFirstResolved,
         edgeAscending: blind && !newestFirstResolved)
    }

    /// Halve a blind chunk on an out-of-range failure: the lower half is
    /// retried first, the upper half queued behind it. nil for a width-1
    /// range (nothing left to halve).
    ///
    /// The no-7 invariant is PROVEN only for widths ≤ 12 — the production
    /// widths (chunkSize 12, halving 12→6+6→3+3→2+1→1+1). Wider inputs may
    /// split into a 7-wide piece, so the caller contract pins width ≤ 12.
    public static func halve(_ range: ClosedRange<Int>) -> (lower: ClosedRange<Int>, upper: ClosedRange<Int>)? {
        precondition(range.count <= 12, "halve() no-7 invariant is only proven for widths <= 12")
        guard range.count > 1 else { return nil }
        let mid = range.lowerBound + range.count / 2 - 1
        return (range.lowerBound...mid, (mid + 1)...range.upperBound)
    }

    /// How to classify a FAILED blind chunk read. Mail ERRORS on
    /// out-of-range reads (-1719 Invalid index, live-verified) — that is
    /// the end-of-mailbox signal: halve at width > 1, stop cleanly at
    /// width 1 (the read frontier). ANY other failure (-1712 timeouts,
    /// executor busy, unusable replies) is transient and takes the strike
    /// budget — halving against a wedged executor would cascade 12→1 in
    /// milliseconds and silently stop the walk.
    public enum BlindReadDecision: Equatable {
        case halve
        case endOfMailbox
        case transientStrike
    }

    public static func blindReadDecision(errorNumber: Int?, width: Int, rangeErrorCode: Int = -1719) -> BlindReadDecision {
        if errorNumber == rangeErrorCode {
            return width > 1 ? .halve : .endOfMailbox
        }
        return .transientStrike
    }

    /// Direction-aware window decision for one dated row. Walks with the
    /// ASCENDING edge (blind oldest-first — dates increase with position)
    /// end ABOVE toMs — half-open: a row exactly AT toMs is out — and their
    /// pre-window rows are ordinary skips (the stale edge must not fire on
    /// them). All other walks (dates decrease with each step — counted
    /// newest-first and counted oldest-first plan layouts) end BELOW
    /// fromMs.
    public enum RowDecision: Equatable {
        case collect
        case skip
        case edge
    }

    public static func rowDecision(ms: Int64, fromMs: Int64, toMs: Int64, edgeAscending: Bool,
                                   staleSoFar: Int, tolerance: Int = 2) -> (decision: RowDecision, stale: Int) {
        if edgeAscending {
            if ms >= toMs {
                let stale = staleSoFar + 1
                return (stale > tolerance ? .edge : .skip, stale)
            }
            return (ms >= fromMs ? .collect : .skip, staleSoFar)
        }
        if ms < fromMs {
            let stale = staleSoFar + 1
            return (stale > tolerance ? .edge : .skip, stale)
        }
        return (ms < toMs ? .collect : .skip, staleSoFar)
    }

    /// Terminal state-file counters: failed bumps the failure streak
    /// (drives the Rust respawn backoff), done zeroes it, cancelled
    /// preserves it; skipped-mailbox counts accumulate for operators.
    public static func terminalCounters(state: String, previousFailures: Int, previousSkipped: Int, newSkips: Int)
        -> (failures: Int?, skipped: Int) {
        let failures: Int?
        switch state {
        case "failed": failures = previousFailures + 1
        case "done": failures = 0
        default: failures = nil  // cancelled and anything else: preserve
        }
        return (failures, previousSkipped + newSkips)
    }

    static func crawlMailbox(jobID: String, account: String, mailboxAt: Int, mailbox: String,
                             window: CrawlWindow, includeContent: Bool,
                             staging: URL, seq: SeqCounter,
                             blindMailboxes: inout Set<String>,
                             progress: @escaping (String, Int, Int) -> Void) -> WalkOutcome {
        func abort(_ scanned: Int, _ indexed: Int, _ reason: String) -> WalkOutcome {
            CrawlState.shared.setFlushPending(nil)
            return WalkOutcome(scanned: scanned, indexed: indexed, transportError: reason, skipReason: nil)
        }
        func skip(_ scanned: Int, _ indexed: Int, _ reason: String) -> WalkOutcome {
            CrawlState.shared.setFlushPending(nil)
            return WalkOutcome(scanned: scanned, indexed: indexed, transportError: nil, skipReason: reason)
        }

        // ── identity of this mailbox-window across windows ──
        let blindKey = "\(account)|\(mailbox)"
        // 12 records per script keeps each execution (~3-5 s/message on the
        // big INBOX) well inside the script's own 240 s event timeout.
        let chunkSize = 12

        var scanned = 0
        var indexed = 0
        var skipped = 0
        var unreadable = 0  // rows that produced no usable record (drives the zero-yield breaker)
        var skippedOutOfWindow = 0  // dated rows outside the window (also feeds the breaker)
        var edgeCrossed = false
        var writeFailed = false
        var orderWasDefaulted = false  // set after the probe; drives the order-undetectable skip
        var orderMisDetectedWalk = false  // set when a defaulted-order descending walk proves itself wrong
        var baseLabel = "\(window.fromMs)-\(window.toMs)"

        func label() -> String {
            skipped > 0 ? "\(baseLabel) skipped=\(skipped)" : baseLabel
        }

        /// fresh state file + stderr line (keeps updated_at alive through
        /// backoff windows and carries the skipped counter)
        func heartbeat() {
            writeState(jobID: jobID, state: "running",
                       processed: CrawlState.shared.snapshot.processed,
                       found: CrawlState.shared.snapshot.found,
                       window: label())
            progress("running", CrawlState.shared.snapshot.processed, CrawlState.shared.snapshot.found)
        }

        // ── count: up to 2 attempts (decision-driven), then BLIND WALK ──
        // AE counts on large mailboxes are O(n) and can exceed any timeout,
        // while range reads on the same mailbox work — a failed count enters
        // blind pagination instead of failing the job. Once a mailbox has
        // gone blind, later windows skip the count attempts entirely
        // (memoized — 2×360 s per window per job would otherwise add up to
        // hours of failed counting).
        var total: Int? = nil
        var countAttempts = 0
        let memoizedBlind = blindMailboxes.contains(blindKey)
        // a memoized-blind mailbox never attempts a count — the diagnostic
        // must say so, not claim an unknown count failure
        var countDiagnostic = memoizedBlind
            ? "blind — count skipped by memoization"
            : "unknown count failure"
        if !memoizedBlind {
            while total == nil {
                if CrawlState.shared.isCancelled { break }
                countAttempts += 1
                if let t = MailAE.countMessages(mailboxAt: mailboxAt, account: account) {
                    total = t
                    blindMailboxes.remove(blindKey)  // Mail calmed — count normally again
                    break
                }
                countDiagnostic = lastError ?? countDiagnostic
                let decision = MailCrawler.countAttemptDecision(tryNumber: countAttempts, total: nil)
                if case .enterBlindWalk = decision {
                    blindMailboxes.insert(blindKey)
                    break  // total stays nil → blind mode
                }
                // .retryAfterBackoff
                heartbeat()
                Thread.sleep(forTimeInterval: 5)
            }
        }
        let blind = memoizedBlind || total == nil
        if blind {
            blindMailboxes.insert(blindKey)
            baseLabel = "blind walk (count failed) \(window.fromMs)-\(window.toMs)"
        }

        guard !blind || MailAE.mailRunning() else {
            return abort(0, 0, "Mail stopped before the blind walk of \(mailbox)")
        }
        if let t = total {
            guard t > 0 else { return WalkOutcome(scanned: 0, indexed: 0, transportError: nil, skipReason: nil) }
        }
        var totalValue = total ?? 0

        // Blind walks have no count: the cap alone bounds the plan, and the
        // walk discovers the mailbox end via short/failed reads.
        let walkLimit = blind ? 20_000 : min(totalValue, 20_000)

        let pending = PendingBatch()
        let flushLock = NSLock()

        func flush() -> Bool {
            flushLock.lock(); defer { flushLock.unlock() }
            let recs = pending.drain()
            guard !recs.isEmpty else { return true }
            if writeBatch(staging: staging, jobID: jobID, seq: seq.value, records: recs) {
                seq.value += 1
                return true
            }
            // put the records back — a later flush (e.g. from the SIGTERM
            // handler) must get another chance at them
            pending.refill(recs)
            return false
        }
        CrawlState.shared.setFlushPending { flush() }

        /// Returns false on a batch-write failure (the walk must stop).
        func collect(_ record: CrawlRecord) -> Bool {
            pending.append(record)
            indexed += 1
            // Small staged batches keep the index fresh while Mail is slow;
            // the JSONL handoff tolerates any record count per file.
            if pending.count() >= 50 {
                return flush()
            }
            return true
        }

        /// Process one chunk: pure scan (mapping + direction-aware window
        /// decision), then fold the results into the walk counters. Returns
        /// true when the window edge is CONFIRMED: more than 2 out-of-window
        /// rows (one mis-dated row must not end the whole window walk).
        /// Direction-aware: descending walks (newest-first mailboxes) end
        /// BELOW fromMs; ascending walks (oldest-first mailboxes, incl. the
        /// forced position-1 blind plan) end ABOVE toMs — pre-window rows
        /// there are ordinary skips, not an edge. Sets
        /// `orderMisDetectedWalk` when a defaulted-order descending walk
        /// proves itself wrong mid-walk (long above-window tail after
        /// collected rows).
        @discardableResult
        func process(_ rows: [NSAppleEventDescriptor?], reverseRows: Bool, edgeAscending: Bool) -> Bool {
            let scan = MailCrawler.scanChunk(rows, window: window, reverseRows: reverseRows,
                                             edgeAscending: edgeAscending, account: account, mailbox: mailbox,
                                             includeContent: includeContent, hasCollected: indexed > 0)
            scanned += scan.scanned
            skipped += scan.skipped
            unreadable += scan.unreadable
            skippedOutOfWindow += scan.skippedOutOfWindow
            if scan.orderMisDetected {
                orderMisDetectedWalk = true
            }
            for record in scan.records where !collect(record) {
                writeFailed = true
            }
            return scan.edgeCrossed
        }

        /// Per-window breaker + order-ambiguity diagnostic. A zero-yield or
        /// defaulted-order window is SKIPPED (never a job failure): other
        /// mailboxes continue and the job must not loop-respawn against
        /// unparseable data.
        func zeroYieldCheck() -> String? {
            if let reason = MailCrawler.zeroYieldSkipReason(unreadable: unreadable, outOfWindow: skippedOutOfWindow,
                                                            indexed: indexed) {
                return reason
            }
            if orderWasDefaulted, indexed == 0 {
                return "skipped: order undetectable — no dated rows to orient the walk"
            }
            return nil
        }

        // First chunk doubles as the order probe (positions shift as mail
        // arrives, but ids dedupe at ingest, so overlap is harmless).
        // walkPlan guarantees no chunk is exactly 7 wide (bundle property
        // count) — such replies are structurally ambiguous.
        let (_, probeRange, _) = MailAE.walkPlan(total: blind ? walkLimit : totalValue,
                                                 walkLimit: walkLimit,
                                                 chunkSize: chunkSize, newestFirst: true,
                                                 avoidWidth: 7)
        let probe: [NSAppleEventDescriptor?]?
        var probeEnd = probeRange.upperBound
        if blind {
            // no total: an out-of-range read ERRORS (-1719 Invalid index),
            // so shrink the probe window until Mail answers. A width-1
            // -1719 means the mailbox is EMPTY → skip with diagnostic;
            // persistent non-range failures take a small strike budget and
            // then abort with the diagnostic.
            var rows: [NSAppleEventDescriptor?]? = nil
            var end = probeRange.upperBound
            var probeStrikes = 0
            while end >= 1 {
                if CrawlState.shared.isCancelled { break }
                rows = MailAE.readProperties(mailboxAt: mailboxAt, account: account, start: 1, end: end,
                                             includeContent: includeContent)
                if rows != nil { probeEnd = end; break }
                if lastErrorNumber == -1719 {
                    if end == 1 {
                        return skip(0, 0, "skipped: mailbox appears empty (count also failed: \(countDiagnostic))")
                    }
                    end = end / 2
                    heartbeat()
                    Thread.sleep(forTimeInterval: 1)
                } else {
                    probeStrikes += 1
                    if probeStrikes >= 2 {
                        return abort(0, 0, "blind probe read failed for \(mailbox) — \(lastError ?? "unknown error")")
                    }
                    heartbeat()
                    Thread.sleep(forTimeInterval: min(60, Double(5 * probeStrikes)))
                }
            }
            probe = rows
        } else {
            probe = MailAE.readProperties(mailboxAt: mailboxAt, account: account,
                                          start: probeRange.lowerBound, end: probeRange.upperBound,
                                          includeContent: includeContent)
        }
        guard let probe, !probe.isEmpty else {
            let why = blind
                ? "count failed (\(countDiagnostic)) and the blind probe read failed for \(mailbox) — \(lastError ?? "unknown error")"
                : "probe read failed for \(mailbox) — \(lastError ?? "unknown error")"
            return abort(0, 0, why)
        }
        var secondSample: Date? = nil
        if probe.count >= 2 { secondSample = MailAE.dateFromRow(probe[1]) }
        var farEndSample: Date? = nil
        // far-end resample only pays a read when the first pair is ambiguous;
        // it needs a total, so blind mode skips it (resolveOrder tolerates nil)
        if !blind,
           MailAE.orderFromSamples(MailAE.dateFromRow(probe[0]), secondSample) == nil,
           totalValue > probeRange.upperBound,
           let lastRows = MailAE.readProperties(mailboxAt: mailboxAt, account: account,
                                                start: totalValue, end: totalValue, includeContent: includeContent),
           !lastRows.isEmpty {
            farEndSample = MailAE.dateFromRow(lastRows[0])
        }
        // resolveOrder recomputes `defaulted` AFTER the resample — a stale
        // defaulted flag falsely skipped resolvable mailboxes
        let (newestFirstResolved, orderWasAmbiguous) = MailCrawler.resolveOrder(
            firstSample: MailAE.dateFromRow(probe[0]),
            secondSample: secondSample,
            farEndSample: farEndSample)
        orderWasDefaulted = orderWasAmbiguous

        // Blind walks FORCE the newest-first plan shape (anchored at
        // position 1, ascending): a far-end-anchored plan would fabricate
        // position `cap` and read out-of-range immediately. Counted walks
        // keep the order-shaped plans.
        //
        // The resolved order drives TWO SEPARATE roles — do not merge them:
        // - reverseRows: reverse within-chunk iteration. Only counted
        //   oldest-first plans are laid out far-end-first (position-
        //   descending), so only they reverse. Blind plans are anchored at
        //   position 1 in BOTH orders and iterate as-is.
        // - edgeAscending: which window bound ends the walk. Only blind
        //   oldest-first walks (ascending dates with position) end ABOVE
        //   toMs; every other walk ends BELOW fromMs. One combined flag
        //   mis-poled counted newest-first backfill windows (0 rows per
        //   window — the round-2 critical regression).
        var (walkRanges, _, processProbe) = MailAE.walkPlan(total: blind ? walkLimit : totalValue,
                                                            walkLimit: walkLimit,
                                                            chunkSize: chunkSize,
                                                            newestFirst: blind ? true : newestFirstResolved,
                                                            avoidWidth: 7)
        let (reverseRows, edgeAscending) = MailCrawler.flagsForWalk(blind: blind,
                                                                    newestFirstResolved: newestFirstResolved)
        if processProbe {
            edgeCrossed = process(probe, reverseRows: reverseRows, edgeAscending: edgeAscending)
            if let reason = zeroYieldCheck() { return skip(scanned, indexed, reason) }
            if orderMisDetectedWalk {
                let flushed = flush()
                CrawlState.shared.setFlushPending(nil)
                if !flushed {
                    return abort(scanned, indexed, "batch write failed for \(mailbox) — \(lastError ?? "unknown error")")
                }
                return skip(scanned, indexed, "skipped: order mis-detected mid-walk — the defaulted order ran against the real layout (\(scanned) rows scanned, \(indexed) indexed)")
            }
            // a SHRUNK blind probe succeeded below the full first chunk:
            // the unread tail would otherwise be skipped — re-queue it
            if blind, probeEnd < probeRange.upperBound {
                var tail = (probeEnd + 1)...probeRange.upperBound
                if tail.count == 7 {
                    // keep the bundle-ambiguity invariant on re-queued tails
                    let halves = MailCrawler.halve(tail)!
                    walkRanges.insert(halves.upper, at: 1)
                    tail = halves.lower
                }
                walkRanges.insert(tail, at: 1)
            }
        }

        var i = processProbe ? 1 : 0  // newest-first: plan[0] IS the probe range
        var consecutiveFailures = 0
        var abortReason: String?
        walk: while i < walkRanges.count, !edgeCrossed, consecutiveFailures < 3, !writeFailed,
              indexed < walkLimit, abortReason == nil {
            if CrawlState.shared.isCancelled {
                CrawlState.shared.signalCancelledObserved()
                break
            }
            let range = walkRanges[i]
            if !blind, range.lowerBound > totalValue {
                i += 1
                continue
            }
            // identity guard: the enumeration order can shift between listing
            // and use — a stale index would silently mislabel every record.
            // A transient nil shares the strike budget; a definitive name
            // mismatch aborts immediately.
            if !MailAE.healthy() {
                consecutiveFailures += 1
                if consecutiveFailures >= 3 {
                    abortReason = "3 consecutive health-check failures in \(mailbox) — \(lastError ?? "Mail unresponsive")"
                    break
                }
                heartbeat()
                Thread.sleep(forTimeInterval: min(60, Double(5 * consecutiveFailures)))
                continue  // retry the same range
            }
            switch MailAE.identityDecision(currentName: MailAE.mailboxName(at: mailboxAt, account: account),
                                           expected: mailbox, mailboxAt: mailboxAt, account: account,
                                           consecutiveFailures: consecutiveFailures) {
            case .verified:
                break
            case .retrySame:
                consecutiveFailures += 1
                heartbeat()
                Thread.sleep(forTimeInterval: min(60, Double(5 * consecutiveFailures)))
                continue  // retry the revalidation (same range)
            case .abort(let reason):
                abortReason = reason
            }
            if abortReason != nil { break }
            if let rows = MailAE.readProperties(mailboxAt: mailboxAt, account: account,
                                                start: range.lowerBound, end: range.upperBound,
                                                includeContent: includeContent) {
                consecutiveFailures = 0
                i += 1
                // gentle pacing: a pause between chunks keeps Mail's event
                // queue responsive for the user's interactive Mail use
                Thread.sleep(forTimeInterval: 1)
                edgeCrossed = process(rows, reverseRows: reverseRows, edgeAscending: edgeAscending)
                if orderMisDetectedWalk {
                    let flushed = flush()
                    CrawlState.shared.setFlushPending(nil)
                    if !flushed {
                        return abort(scanned, indexed, "batch write failed for \(mailbox) — \(lastError ?? "unknown error")")
                    }
                    return skip(scanned, indexed, "skipped: order mis-detected mid-walk — the defaulted order ran against the real layout (\(scanned) rows scanned, \(indexed) indexed)")
                }
                heartbeat()
                if let reason = zeroYieldCheck() {
                    // skip this mailbox's window (diagnostic in the state
                    // label) and let the remaining mailboxes continue —
                    // but a batch-write failure still fails the job
                    let flushed = flush()
                    CrawlState.shared.setFlushPending(nil)
                    if !flushed {
                        return abort(scanned, indexed, "batch write failed for \(mailbox) — \(lastError ?? "unknown error")")
                    }
                    return skip(scanned, indexed, reason)
                }
                if MailCrawler.isShortRead(rows: rows.count, requested: range.count, blind: blind) {
                    // blind mode: a chunk returning FEWER rows than asked
                    // means the mailbox is exhausted — what came back is
                    // already processed; stop the walk cleanly
                    break
                }
            } else if blind {
                // Blind pagination with no known total: classify the failure.
                // -1719 (Invalid index) at the read frontier is the
                // end-of-mailbox signal — halve at width > 1 (halving from 12
                // never produces a 7-wide bundle-ambiguous piece), stop
                // cleanly at width 1. ANY other failure is transient and
                // takes the strike budget (halving against a wedged executor
                // would cascade 12→1 in milliseconds and silently stop).
                heartbeat()
                switch MailCrawler.blindReadDecision(errorNumber: lastErrorNumber,
                                                     width: range.count) {
                case .halve:
                    let halves = MailCrawler.halve(range)!
                    walkRanges[i] = halves.lower
                    walkRanges.insert(halves.upper, at: i + 1)
                    Thread.sleep(forTimeInterval: 1)
                case .endOfMailbox:
                    break walk
                case .transientStrike:
                    consecutiveFailures += 1
                    if consecutiveFailures >= 3 {
                        abortReason = "3 consecutive blind-read failures in \(mailbox) at \(range.lowerBound)-\(range.upperBound) — \(lastError ?? "unknown error")"
                    } else {
                        Thread.sleep(forTimeInterval: min(60, Double(5 * consecutiveFailures)))
                    }
                }
            } else {
                // refresh the count first: ranges beyond a shrunken mailbox
                // evaporate instead of counting as transport failures
                let refreshed = MailAE.countMessages(mailboxAt: mailboxAt, account: account)
                if let nt = refreshed, nt != totalValue { totalValue = nt }
                switch MailAE.failureDecision(range: range, refreshedTotal: refreshed,
                                              consecutiveFailures: consecutiveFailures + 1) {
                case .advance:
                    i += 1
                    consecutiveFailures = 0  // fresh strike budget for the next range
                case .abort:
                    consecutiveFailures = 3
                    abortReason = "3 consecutive chunk-read failures in \(mailbox) at \(range.lowerBound)-\(range.upperBound) — \(lastError ?? "unknown error")"
                case .retrySame:
                    consecutiveFailures += 1
                    heartbeat()
                    Thread.sleep(forTimeInterval: min(60, Double(5 * consecutiveFailures)))
                }
            }
        }
        if CrawlState.shared.isCancelled {
            CrawlState.shared.signalCancelledObserved()
        }
        let flushed = flush()
        CrawlState.shared.setFlushPending(nil)
        if writeFailed || !flushed {
            return abort(scanned, indexed, "batch write failed for \(mailbox) — \(lastError ?? "unknown error")")
        }
        if let abortReason {
            return abort(scanned, indexed, abortReason)
        }
        return WalkOutcome(scanned: scanned, indexed: indexed, transportError: nil, skipReason: nil)
    }

    /// Synchronous crawl used by the detached `bite-crawl` worker process.
    /// Returns the terminal state ("done"/"failed"/"cancelled") plus the
    /// batch sequence counter so mirror crawls continue the same job's
    /// numbering instead of colliding with mail batches. jobID comes from
    /// the caller — one source of truth for the state file and batch names.
    public static func runCrawlWorker(jobID: String, windowDays: Int, storeBody: Bool,
                                      mailboxFilter: String?,
                                      progress: @escaping (String, Int, Int) -> Void)
        -> (state: String, seq: SeqCounter) {
        let seq = SeqCounter(0)
        writeState(jobID: jobID, state: "running", processed: 0, found: 0, window: nil)
        let terminal = runJob(jobID: jobID, windowDays: windowDays, storeBody: storeBody,
                              mailboxFilter: mailboxFilter, seq: seq, progress: progress)
        return (terminal, seq)
    }
}
