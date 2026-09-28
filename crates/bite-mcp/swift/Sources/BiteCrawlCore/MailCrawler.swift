// Mail crawler core: window scheduling (30d initial + 10d backfill),
// chunked OSA reads, JSONL batch staging.
//
// Runs inside the standalone `bite-crawl` process. Lifetime is fully
// detached from the helper/CLI/MCP: progress is published through a
// callback + a crawl-state.json file the Rust control plane polls.
//
// Mail transport: every read goes through MailAE's serialized NSAppleScript
// executor (raw Apple Event sending is broken on macOS 27). Message reads
// fetch `properties` in small index ranges — one bundled get per message,
// bounded well under Mail's 120 s AppleEvent timeout.

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

public final class CrawlState {
    public static let shared = CrawlState()
    private let lock = NSLock()
    private var cancelled = false
    private var processed = 0
    private var found = 0
    private var window: String?

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

    public static func writeBatch(staging: URL, jobID: String, seq: Int, records: [CrawlRecord]) {
        let path = staging.appendingPathComponent("\(jobID)-\(seq).jsonl")
        var lines = ""
        for r in records {
            if let data = try? JSONEncoder().encode(r), let line = String(data: data, encoding: .utf8) {
                lines += line + "\n"
            }
        }
        let tmp = path.appendingPathExtension("tmp")
        try? lines.data(using: .utf8)?.write(to: tmp)
        try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: tmp.path)
        try? FileManager.default.moveItem(at: tmp, to: path)
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
        let path = statePath()
        if let data = try? JSONSerialization.data(withJSONObject: dict, options: [.prettyPrinted]) {
            let tmp = path.appendingPathExtension("tmp")
            try? data.write(to: tmp)
            try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: tmp.path)
            try? FileManager.default.removeItem(at: path)
            try? FileManager.default.moveItem(at: tmp, to: path)
        }
    }

    public static func runJob(jobID: String, windowDays: Int, storeBody: Bool, mailboxFilter: String?,
                              seq: SeqCounter, progress: @escaping (String, Int, Int) -> Void) {
        let state = CrawlState.shared
        guard MailAE.mailRunning() else {
            writeState(jobID: jobID, state: "failed", processed: 0, found: 0, window: nil)
            progress("failed", 0, 0)
            return
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
            return
        }

        var targets: [(account: String, name: String)] = []
        for acct in accounts {
            guard let boxes = MailAE.mailboxList(account: acct) else { continue }
            for boxName in boxes {
                if let filter = mailboxFilter,
                   boxName.caseInsensitiveCompare(filter) != .orderedSame { continue }
                targets.append((account: acct, name: boxName))
            }
        }

        var processed = 0
        var found = 0
        let staging = stagingDir()
        let windowLabel = { (w: CrawlWindow) -> String in "\(w.fromMs)-\(w.toMs)" }

        for window in windows {
            for t in targets {
                if state.isCancelled { break }
                let label = windowLabel(window)
                writeState(jobID: jobID, state: "running", processed: processed, found: found, window: label)
                progress("running", processed, found)
                let r = crawlMailbox(jobID: jobID, account: t.account, mailbox: t.name,
                                     window: window, includeContent: storeBody,
                                     staging: staging, seq: seq, progress: progress)
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
    }

    /// Walk one mailbox for one date window in index-range chunks. Order
    /// detected from the first chunk's dated rows; the walk stops at the
    /// first window edge crossed.
    static func crawlMailbox(jobID: String, account: String, mailbox: String,
                             window: CrawlWindow, includeContent: Bool,
                             staging: URL, seq: SeqCounter,
                             progress: @escaping (String, Int, Int) -> Void) -> (scanned: Int, indexed: Int) {
        guard let total = MailAE.countMessages(mailbox: mailbox, account: account), total > 0 else {
            return (0, 0)
        }

        // 12 full records per script keeps each execution (~3-5 s/message on
        // the big INBOX) well inside the script's own 240 s event timeout.
        let chunkSize = 12
        let walkLimit = min(total, 20_000)

        var batchRecords: [CrawlRecord] = []
        var scanned = 0
        var indexed = 0
        var edgeCrossed = false

        func flush() {
            guard !batchRecords.isEmpty else { return }
            writeBatch(staging: staging, jobID: jobID, seq: seq.value, records: batchRecords)
            seq.value += 1
            batchRecords.removeAll(keepingCapacity: true)
        }

        func collect(_ record: CrawlRecord) {
            batchRecords.append(record)
            indexed += 1
            // Small staged batches keep the index fresh while Mail is slow;
            // the JSONL handoff tolerates any record count per file.
            if batchRecords.count >= 50 {
                flush()
            }
        }

        func inWindow(_ ms: Int64) -> Bool {
            ms >= window.fromMs && ms < window.toMs
        }

        // Process one chunk's rows in walk order. Returns true when the
        // window's lower edge was crossed.
        func process(_ rows: [NSAppleEventDescriptor?], descending: Bool) -> Bool {
            for row in descending ? rows.reversed() : rows {
                scanned += 1
                guard let record = MailAE.recordFromProperties(row, account: account, mailbox: mailbox,
                                                               includeContent: includeContent),
                      let ms = record.start_ms else { continue }
                if ms < window.fromMs { return true }
                if inWindow(ms) { collect(record) }
            }
            return false
        }

        func rowDate(_ row: NSAppleEventDescriptor?) -> Date? {
            row?.forKeyword(MailAE.kwDateSent)?.dateValue
        }

        // First chunk doubles as the order probe (positions shift as mail
        // arrives, but ids dedupe at ingest, so overlap is harmless).
        let probeEnd = min(chunkSize, walkLimit)
        guard let probe = MailAE.readProperties(mailbox: mailbox, account: account, start: 1, end: probeEnd,
                                                includeContent: includeContent), !probe.isEmpty else {
            return (0, 0)
        }
        var newestFirst = true
        if probe.count >= 2, let d1 = rowDate(probe[0]), let d2 = rowDate(probe[1]) {
            newestFirst = d1 >= d2
        }

        var plan = MailAE.chunkPlan(total: total, walkLimit: walkLimit, chunkSize: chunkSize, newestFirst: newestFirst)
        if !newestFirst {
            plan.removeFirst()  // probe chunk holds the mailbox's oldest rows; the descending walk starts at the far end
        }

        if newestFirst {
            edgeCrossed = process(probe, descending: false)
        }

        var consecutiveFailures = 0
        for range in plan {
            if CrawlState.shared.isCancelled || edgeCrossed || consecutiveFailures >= 3 || indexed >= walkLimit {
                break
            }
            if !MailAE.healthy() {
                consecutiveFailures += 1
                Thread.sleep(forTimeInterval: min(60, Double(5 * consecutiveFailures)))
                continue
            }
            if let rows = MailAE.readProperties(mailbox: mailbox, account: account, start: range.start, end: range.end,
                                                includeContent: includeContent) {
                consecutiveFailures = 0
                // gentle pacing: a pause between chunks keeps Mail's event
                // queue responsive for the user's interactive Mail use
                Thread.sleep(forTimeInterval: 1)
                edgeCrossed = process(rows, descending: !newestFirst)
                progress("running", CrawlState.shared.snapshot.processed, CrawlState.shared.snapshot.found)
            } else {
                consecutiveFailures += 1
                Thread.sleep(forTimeInterval: min(60, Double(5 * consecutiveFailures)))
            }
        }
        flush()
        return (scanned, indexed)
    }

    /// Synchronous crawl used by the detached `bite-crawl` worker process.
    /// Returns the batch sequence counter so mirror crawls continue the
    /// same job's numbering instead of colliding with mail batches.
    @discardableResult
    public static func runCrawlWorker(windowDays: Int, storeBody: Bool, mailboxFilter: String?,
                                      progress: @escaping (String, Int, Int) -> Void) -> SeqCounter {
        let jobID = "crawl-\(Int(Date().timeIntervalSince1970))"
        let seq = SeqCounter(0)
        writeState(jobID: jobID, state: "running", processed: 0, found: 0, window: nil)
        runJob(jobID: jobID, windowDays: windowDays, storeBody: storeBody, mailboxFilter: mailboxFilter,
               seq: seq, progress: progress)
        return seq
    }
}
