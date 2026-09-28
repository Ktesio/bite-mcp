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
/// can flush a partial batch without racing the crawl thread.
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
}

public final class CrawlState {
    public static let shared = CrawlState()
    private let lock = NSLock()
    private var cancelled = false
    private var processed = 0
    private var found = 0
    private var window: String?
    private var flushPending: (() -> Void)?

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
    /// a cancel never strands up to 50 staged records.
    public func setFlushPending(_ f: (() -> Void)?) {
        lock.lock(); defer { lock.unlock() }
        flushPending = f
    }

    public func runFlushPending() {
        lock.lock(); let f = flushPending; lock.unlock()
        f?()
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

    public static func writeState(jobID: String, state: String, processed: Int, found: Int, window: String?) {
        CrawlState.shared.setWindow(window)
        let dict: [String: Any] = [
            "job_id": jobID,
            "state": state,
            "processed": processed,
            "found": found,
            "window": window ?? NSNull(),
            "updated_at": ISO8601DateFormatter().string(from: Date()),
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: dict, options: [.prettyPrinted]) else { return }
        stateLock.lock()
        defer { stateLock.unlock() }
        let path = statePath()
        let tmp = path.appendingPathExtension("\(ProcessInfo.processInfo.globallyUniqueString).tmp")
        guard FileManager.default.createFile(atPath: tmp.path, contents: data,
                                             attributes: [.posixPermissions: 0o600]) else { return }
        _ = atomicReplace(tmp: tmp, dst: path)
    }

    /// Remove stale *.tmp droppings from a previous crashed run before this
    /// job starts staging.
    public static func sweepStaleTmp(at staging: URL) {
        let fm = FileManager.default
        guard let items = try? fm.contentsOfDirectory(at: staging, includingPropertiesForKeys: nil) else { return }
        for f in items where f.lastPathComponent.hasSuffix(".tmp") {
            try? fm.removeItem(at: f)
        }
    }

    public static func runJob(jobID: String, windowDays: Int, storeBody: Bool, mailboxFilter: String?,
                              seq: SeqCounter, progress: @escaping (String, Int, Int) -> Void) -> String {
        let state = CrawlState.shared
        guard MailAE.mailRunning() else {
            writeState(jobID: jobID, state: "failed", processed: 0, found: 0, window: "Mail is not running")
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
                       window: "Mail unresponsive for 24 h")
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
                       window: "cannot list mailboxes — \(diag)")
            progress("failed", 0, 0)
            return "failed"
        }

        let staging = stagingDir()
        sweepStaleTmp(at: staging)

        var processed = 0
        var found = 0
        let windowLabel = { (w: CrawlWindow) -> String in "\(w.fromMs)-\(w.toMs)" }

        for window in windows {
            for t in targets {
                if state.isCancelled { break }
                let label = windowLabel(window)
                writeState(jobID: jobID, state: "running", processed: processed, found: found, window: label)
                progress("running", processed, found)
                let r = crawlMailbox(jobID: jobID, account: t.account, mailboxAt: t.index, mailbox: t.name,
                                     window: window, includeContent: storeBody,
                                     staging: staging, seq: seq, progress: progress)
                if let err = r.transportError {
                    // a transport failure is a failed JOB, not a skipped
                    // mailbox — silently walking on would report done with
                    // holes in the index
                    writeState(jobID: jobID, state: "failed",
                               processed: processed + r.scanned, found: found + r.indexed,
                               window: err)
                    progress("failed", processed + r.scanned, found + r.indexed)
                    return "failed"
                }
                processed += r.scanned
                found += r.indexed
                CrawlState.shared.tally(processedAdd: r.scanned, foundAdd: r.indexed)
                progress("running", processed, found)
            }
            if state.isCancelled { break }
        }

        let finalState = state.isCancelled ? "cancelled" : "done"
        writeState(jobID: jobID, state: finalState, processed: processed, found: found, window: nil)
        progress(finalState, processed, found)
        return finalState
    }

    /// Walk one mailbox for one date window in index-range chunks. Order
    /// detected from dated samples (resampled against the far end when the
    /// first pair is ambiguous); the walk stops at the first confirmed
    /// window edge. A failed chunk retries the SAME range — transient
    /// errors never silently drop messages.
    static func crawlMailbox(jobID: String, account: String, mailboxAt: Int, mailbox: String,
                             window: CrawlWindow, includeContent: Bool,
                             staging: URL, seq: SeqCounter,
                             progress: @escaping (String, Int, Int) -> Void)
        -> (scanned: Int, indexed: Int, transportError: String?) {
        guard let initialTotal = MailAE.countMessages(mailboxAt: mailboxAt, account: account) else {
            return (0, 0, "count failed for \(mailbox) — \(lastError ?? "unknown error")")
        }
        guard initialTotal > 0 else { return (0, 0, nil) }
        var total = initialTotal

        // 12 records per script keeps each execution (~3-5 s/message on the
        // big INBOX) well inside the script's own 240 s event timeout.
        let chunkSize = 12
        let walkLimit = min(total, 20_000)
        let baseLabel = "\(window.fromMs)-\(window.toMs)"

        let pending = PendingBatch()
        let flushLock = NSLock()

        func flush() -> Bool {
            flushLock.lock(); defer { flushLock.unlock() }
            let recs = pending.drain()
            guard !recs.isEmpty else { return true }
            let ok = writeBatch(staging: staging, jobID: jobID, seq: seq.value, records: recs)
            if ok {
                seq.value += 1
            }
            return ok
        }
        CrawlState.shared.setFlushPending { _ = flush() }

        var scanned = 0
        var indexed = 0
        var skipped = 0
        var edgeCrossed = false
        var writeFailed = false

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

        func rowDate(_ row: NSAppleEventDescriptor?) -> Date? {
            row?.forKeyword(MailAE.kwDateSent)?.dateValue
        }

        // Process one chunk's rows in walk order. Returns true when the
        // window edge is CONFIRMED: more than 2 stale rows (one mis-dated
        // row must not end the whole window walk).
        @discardableResult
        func process(_ rows: [NSAppleEventDescriptor?], descending: Bool) -> Bool {
            let edgeTolerance = 2
            var stale = 0
            for row in descending ? rows.reversed() : rows {
                scanned += 1
                guard let record = MailAE.recordFromRow(row, account: account, mailbox: mailbox,
                                                        includeContent: includeContent),
                      let ms = record.start_ms else {
                    skipped += 1
                    continue
                }
                if ms < window.fromMs {
                    stale += 1
                    skipped += 1
                    if stale > edgeTolerance { return true }
                    continue
                }
                if ms < window.toMs, !collect(record) {
                    writeFailed = true
                }
            }
            return false
        }

        // First chunk doubles as the order probe (positions shift as mail
        // arrives, but ids dedupe at ingest, so overlap is harmless).
        let probeEnd = min(chunkSize, walkLimit, total)
        guard let probe = MailAE.readProperties(mailboxAt: mailboxAt, account: account, start: 1, end: probeEnd,
                                                includeContent: includeContent), !probe.isEmpty else {
            CrawlState.shared.setFlushPending(nil)
            return (0, 0, "probe read failed for \(mailbox) — \(lastError ?? "unknown error")")
        }
        var newestFirst = MailAE.orderFromSamples(rowDate(probe[0]), probe.count >= 2 ? rowDate(probe[1]) : nil)
        if newestFirst == nil, total > probeEnd,
           let lastRows = MailAE.readProperties(mailboxAt: mailboxAt, account: account,
                                                start: total, end: total, includeContent: includeContent),
           let lastRow = lastRows.first {
            // ambiguous first pair — resample against the far end
            newestFirst = MailAE.orderFromSamples(rowDate(probe[0]), rowDate(lastRow))
        }
        let newestFirstResolved = newestFirst ?? true

        let (plan, processProbeFirst) = MailAE.walkPlan(total: total, walkLimit: walkLimit,
                                                        chunkSize: chunkSize, newestFirst: newestFirstResolved)
        if processProbeFirst {
            edgeCrossed = process(probe, descending: false)
        }

        var i = processProbeFirst ? 1 : 0  // plan[0] IS the probe range for newest-first walks
        var consecutiveFailures = 0
        while i < plan.count, !CrawlState.shared.isCancelled, !edgeCrossed,
              consecutiveFailures < 3, !writeFailed, indexed < walkLimit {
            let range = plan[i]
            if range.lowerBound > total {
                i += 1
                continue
            }
            if !MailAE.healthy() {
                consecutiveFailures += 1
                if consecutiveFailures >= 3 { break }
                heartbeat()
                Thread.sleep(forTimeInterval: min(60, Double(5 * consecutiveFailures)))
                continue  // retry the same range
            }
            if let rows = MailAE.readProperties(mailboxAt: mailboxAt, account: account,
                                                start: range.lowerBound, end: range.upperBound,
                                                includeContent: includeContent) {
                consecutiveFailures = 0
                i += 1
                // gentle pacing: a pause between chunks keeps Mail's event
                // queue responsive for the user's interactive Mail use
                Thread.sleep(forTimeInterval: 1)
                edgeCrossed = process(rows, descending: !newestFirstResolved)
                heartbeat()
            } else {
                // refresh the count first: ranges beyond a shrunken mailbox
                // evaporate instead of counting as transport failures
                let refreshed = MailAE.countMessages(mailboxAt: mailboxAt, account: account)
                if let nt = refreshed, nt != total { total = nt }
                switch MailAE.failureDecision(range: range, refreshedTotal: refreshed,
                                              consecutiveFailures: consecutiveFailures + 1) {
                case .advance:
                    i += 1
                case .abort:
                    consecutiveFailures = 3
                case .retrySame:
                    consecutiveFailures += 1
                    heartbeat()
                    Thread.sleep(forTimeInterval: min(60, Double(5 * consecutiveFailures)))
                }
            }
        }
        CrawlState.shared.setFlushPending(nil)
        if writeFailed || !flush() {
            return (scanned, indexed, "batch write failed for \(mailbox) — \(lastError ?? "unknown error")")
        }
        return (scanned, indexed, nil)
    }

    /// Synchronous crawl used by the detached `bite-crawl` worker process.
    /// Returns the terminal state ("done"/"failed"/"cancelled") plus the
    /// batch sequence counter so mirror crawls continue the same job's
    /// numbering instead of colliding with mail batches.
    public static func runCrawlWorker(windowDays: Int, storeBody: Bool, mailboxFilter: String?,
                                      progress: @escaping (String, Int, Int) -> Void)
        -> (state: String, seq: SeqCounter) {
        let jobID = "crawl-\(Int(Date().timeIntervalSince1970))"
        let seq = SeqCounter(0)
        writeState(jobID: jobID, state: "running", processed: 0, found: 0, window: nil)
        let terminal = runJob(jobID: jobID, windowDays: windowDays, storeBody: storeBody,
                              mailboxFilter: mailboxFilter, seq: seq, progress: progress)
        return (terminal, seq)
    }
}
