// Mail crawler core: window scheduling (30d initial + 10d backfill),
// health-probed per-message reads, JSONL batch staging.
//
// Runs inside the standalone `bite-crawl` process. Lifetime is fully
// detached from the helper/CLI/MCP: progress is published through a
// callback + a crawl-state.json file the Rust control plane polls.

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
                              progress: @escaping (String, Int, Int) -> Void) {
        let state = CrawlState.shared
        guard let target = MailAE.mailTarget() else {
            writeState(jobID: jobID, state: "failed", processed: 0, found: 0, window: nil)
            progress("failed", 0, 0)
            return
        }
        let props: [FourCharCode] = storeBody
            ? [MailAE.pID, MailAE.pSubject, MailAE.pSender, MailAE.pDateSent, MailAE.pRead, MailAE.pFlagged, MailAE.pJunk, MailAE.pContent]
            : [MailAE.pID, MailAE.pSubject, MailAE.pSender, MailAE.pDateSent, MailAE.pRead, MailAE.pFlagged, MailAE.pJunk]

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
        var accounts: [(name: String, spec: NSAppleEventDescriptor)]? = nil
        let maxWaitAttempts = 1440  // 24 h at 60 s intervals
        for attempt in 0..<maxWaitAttempts {
            if CrawlState.shared.isCancelled { break }
            if let list = MailAE.accountList(target: target) {
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

        var targets: [(account: String, name: String, spec: NSAppleEventDescriptor)] = []
        for acct in accounts {
            guard let boxes = MailAE.mailboxList(accountName: acct.name, account: acct.spec, target: target) else { continue }
            for (boxName, boxSpec) in boxes {
                if let filter = mailboxFilter,
                   boxName.caseInsensitiveCompare(filter) != .orderedSame { continue }
                targets.append((account: acct.name, name: boxName, spec: boxSpec))
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
                let r = crawlMailbox(jobID: jobID, account: t.name, mailbox: t.name, boxSpec: t.spec,
                                     window: window, props: props, storeBody: storeBody,
                                     target: target, staging: staging, progress: progress)
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

    /// Walk one mailbox for one date window. Order detected from the first
    /// two dated samples; walk stops at the first window edge crossed.
    static func crawlMailbox(jobID: String, account: String, mailbox: String, boxSpec: NSAppleEventDescriptor,
                             window: CrawlWindow, props: [FourCharCode], storeBody: Bool,
                             target: NSAppleEventDescriptor, staging: URL,
                             progress: @escaping (String, Int, Int) -> Void) -> (scanned: Int, indexed: Int) {
        guard let total = MailAE.countMessages(mailbox: boxSpec, target: target), total > 0 else {
            return (0, 0)
        }

        func read(_ position: Int) -> CrawlRecord? {
            guard let vals = MailAE.readProperties(props, of: MailAE.messageSpec(index: position, mailbox: boxSpec), target: target),
                  vals.count == props.count else {
                return nil
            }
            func str(_ i: Int) -> String? { vals[i]?.stringValue }
            func bool(_ i: Int) -> Bool? { vals[i]?.booleanValue }
            func ms(_ i: Int) -> Int64? { vals[i]?.dateValue.map { Int64($0.timeIntervalSince1970 * 1000) } }
            var mailID = str(0) ?? ""
            if mailID.isEmpty, let d0 = vals[0] {
                mailID = String(d0.int32Value)
            }
            guard !mailID.isEmpty, mailID != "0" else { return nil }
            return CrawlRecord(
                app: "mail", id: mailID, account: account, container: mailbox,
                title: str(1), content: storeBody ? str(7) : nil, participants: str(2),
                start_ms: ms(3), end_ms: nil, updated_ms: ms(3),
                read: bool(4), flagged: bool(5), junk: bool(6),
                completed: nil, priority: nil, props: nil
            )
        }

        func dated(_ position: Int) -> Date? {
            guard let vals = MailAE.readProperties([MailAE.pDateSent], of: MailAE.messageSpec(index: position, mailbox: boxSpec), target: target),
                  vals.count == 1 else { return nil }
            return vals[0]?.dateValue
        }

        var newestFirst = true
        if let d1 = dated(1), let d2 = dated(2), total >= 2 {
            newestFirst = d1 >= d2
        }

        var batchRecords: [CrawlRecord] = []
        var batchSeq = 0
        var scanned = 0
        var indexed = 0
        var failures = 0
        var edgeCrossed = false
        let walkLimit = min(total, 20_000)

        func collect(_ record: CrawlRecord) {
            batchRecords.append(record)
            indexed += 1
            if batchRecords.count >= 500 {
                writeBatch(staging: staging, jobID: jobID, seq: batchSeq, records: batchRecords)
                batchSeq += 1
                batchRecords.removeAll(keepingCapacity: true)
            }
        }

        func inWindow(_ ms: Int64) -> Bool {
            ms >= window.fromMs && ms < window.toMs
        }

        if newestFirst {
            var position = 1
            while position <= walkLimit, !CrawlState.shared.isCancelled,
                  !edgeCrossed, failures < 10, indexed < walkLimit {
                if !MailAE.healthy(target: target) {
                    failures += 1
                    Thread.sleep(forTimeInterval: min(300, Double(10 * failures)))
                    continue
                }
                guard let record = read(position) else {
                    failures += 1
                    position += 1
                    continue
                }
                scanned += 1
                position += 1
                if let ms = record.start_ms, ms < window.fromMs { edgeCrossed = true; break }
                if let ms = record.start_ms, inWindow(ms) { collect(record) }
                if CrawlState.shared.snapshot.processed % 25 == 0 {
                    progress("running", CrawlState.shared.snapshot.processed, CrawlState.shared.snapshot.found)
                }
            }
        } else {
            var position = total
            let floor = max(1, total - walkLimit)
            while position >= floor, !CrawlState.shared.isCancelled,
                  !edgeCrossed, failures < 10, indexed < walkLimit {
                if !MailAE.healthy(target: target) {
                    failures += 1
                    Thread.sleep(forTimeInterval: min(300, Double(10 * failures)))
                    continue
                }
                guard let record = read(position) else {
                    failures += 1
                    position -= 1
                    continue
                }
                scanned += 1
                position -= 1
                if let ms = record.start_ms {
                    if ms < window.fromMs { edgeCrossed = true; break }
                }
                if let ms = record.start_ms, inWindow(ms) { collect(record) }
                if CrawlState.shared.snapshot.processed % 25 == 0 {
                    progress("running", CrawlState.shared.snapshot.processed, CrawlState.shared.snapshot.found)
                }
            }
        }
        if !batchRecords.isEmpty {
            writeBatch(staging: staging, jobID: jobID, seq: batchSeq, records: batchRecords)
        }
        return (scanned, indexed)
    }
}
