// OSA (NSAppleScript) transport for Mail — re-implementation of the raw
// Apple Event toolkit after macOS 27 broke direct AE sending.
//
// Why: on macOS 27, `NSAppleEventDescriptor.sendEvent` and raw
// `AESendMessage` return silent empty null() replies for ALL events from
// non-OSA senders (verified with minimal repros against Mail AND TextEdit:
// literal-string `getd` echoes come back null; AESendMessage gets errn
// -1700). OSA-mediated sending — NSAppleScript, ScriptingBridge, osascript —
// works, so every Mail interaction here is a compiled script executed
// through `OSAExecutor`.
//
// Shape constraints baked into this module:
// - NSAppleScript is not thread-safe: ALL executions run on ONE serial
//   queue. Every submission is watchdog-guarded; on overrun the execution
//   is abandoned (the NSAppleScript leaks rather than deadlocking) and a
//   timeout error is surfaced. While the executor is wedged, further
//   submissions fail fast instead of piling up behind the stuck script.
// - EVERY script wraps its work in `with timeout of N seconds` with N
//   strictly below the caller's watchdog, so Mail's default 120 s
//   AppleEvent timeout can never keep the executor blocked ~2 minutes
//   after the watchdog has already given up.
// - Message reads are ONE Apple Event per chunk. `get properties` fetches
//   full records (content included); a 7-property explicit bundle is used
//   when bodies are not wanted (bodies cost ~half the read time). Loops
//   that fetch properties in separate accesses wedge Mail's AE queue.
// - Mail's mailboxes are ACCOUNT-scoped: always account → mailbox →
//   messages. No script ever targets a stopped Mail (guarding with
//   `mailRunning()` prevents NSAppleScript auto-launching it).

import Foundation
import AppKit

/// Last Mail transport error (script error message or watchdog overrun).
public var lastError: String?

// Message record keywords (sdef four-char codes, validated live on macOS 27).
let kwMessageIDD = AEKeyword(0x49442020)      // 'ID  '  — id
let kwSubjectD = AEKeyword(0x7375626a)        // 'subj'  — subject
let kwSenderD = AEKeyword(0x736e6472)         // 'sndr'  — sender
let kwDateSentD = AEKeyword(0x64726376)       // 'drcv'  — date sent
let kwReadD = AEKeyword(0x69737264)           // 'isrd'  — read status
let kwFlaggedD = AEKeyword(0x6973666c)        // 'isfl'  — flagged status
let kwJunkD = AEKeyword(0x69736a6b)           // 'isjk'  — junk mail status
let kwContentD = AEKeyword(0x63746e74)        // 'ctnt'  — content

// Descriptor types we must tell apart when mapping replies.
let typeListD = DescType(0x6c697374)          // 'list'
let typeRecordD = DescType(0x7265636f)        // 'reco'
let typeDoubleD = DescType(0x646f7562)        // 'doub'
let typeCompD = DescType(0x636f6d70)          // 'comp' — 64-bit integer

/// Serialized NSAppleScript executor. All OSA work funnels through the one
/// dedicated queue; `run` blocks the caller up to `timeoutSeconds` and
/// returns nil (with `lastError` set) on watchdog overrun or script error.
final class OSAExecutor {
    static let shared = OSAExecutor()
    private let queue = DispatchQueue(label: "bite.osa.mail", qos: .userInitiated)
    private let lock = NSLock()
    private var inFlight = false

    private final class ResultBox {
        var reply: NSAppleEventDescriptor?
        var failure: String?
    }

    /// Compile + execute one script. Runs on the executor queue only.
    private func executeScript(_ source: String) -> (NSAppleEventDescriptor?, String?) {
        let script = NSAppleScript(source: source)
        var err: NSDictionary?
        if let desc = script?.executeAndReturnError(&err) {
            return (desc, nil)
        }
        // Documented NSAppleScriptErrorDictionary keys (the SDK's constant
        // declarations don't play well with dictionary subscripts here)
        let num = (err?["NSAppleScriptErrorNumber"] as? Int).map(String.init) ?? "?"
        let msg = (err?["NSAppleScriptErrorMessage"] as? String) ?? "unknown OSA failure"
        return (nil, "AppleScript error \(num): \(msg)")
    }

    func run(_ source: String, timeoutSeconds: TimeInterval) -> NSAppleEventDescriptor? {
        lock.lock()
        if inFlight {
            // A previous script is still executing (likely wedged on Mail).
            // Fail fast instead of queueing behind it — piled-up scripts
            // would keep hammering Mail after the wedge clears.
            lock.unlock()
            lastError = "osa executor busy — previous script still in flight"
            return nil
        }
        inFlight = true
        lock.unlock()

        let sem = DispatchSemaphore(value: 0)
        let box = ResultBox()
        let work = DispatchWorkItem {
            let (reply, failure) = self.executeScript(source)
            box.reply = reply
            box.failure = failure
            self.lock.lock()
            self.inFlight = false
            self.lock.unlock()
            sem.signal()
        }
        queue.async(execute: work)
        if sem.wait(timeout: .now() + timeoutSeconds) == .timedOut {
            // Abandon this execution: the executor thread stays blocked on
            // Mail (the NSAppleScript leaks), `inFlight` keeps further
            // submissions failing fast, and the caller gets a timeout now
            // instead of a deadlock. The in-script `with timeout` guarantees
            // the blocked event errors out well before the watchdog would.
            lastError = "AppleScript timeout after \(Int(timeoutSeconds))s — Mail unresponsive"
            return nil
        }
        if let failure = box.failure {
            lastError = failure
            return nil
        }
        lastError = nil
        return box.reply
    }
}

public enum MailAE {
    // ── shared keywords (public for record mapping + tests) ──

    public static let kwMessageID = kwMessageIDD
    public static let kwSubject = kwSubjectD
    public static let kwSender = kwSenderD
    public static let kwDateSent = kwDateSentD
    public static let kwRead = kwReadD
    public static let kwFlagged = kwFlaggedD
    public static let kwJunk = kwJunkD
    public static let kwContent = kwContentD

    /// Properties fetched by the no-body bundle, in bundle order.
    static let bundleProps = "id, subject, sender, date sent, read status, flagged status, junk mail status"

    // ── script text builders (pure; unit-tested) ──

    /// AppleScript string literal: escapes backslash, quote, and the
    /// representable control characters; other scalars below 0x20 are
    /// stripped (AppleScript has no hex escapes and they never belong in
    /// mailbox/account names).
    public static func quotedAppleString(_ s: String) -> String {
        var out = "\""
        for scalar in s.unicodeScalars {
            switch scalar {
            case "\"": out += "\\\""
            case "\\": out += "\\\\"
            case "\n": out += "\\n"
            case "\r": out += "\\r"
            case "\t": out += "\\t"
            default:
                if scalar.value >= 0x20 { out.unicodeScalars.append(scalar) }
            }
        }
        return out + "\""
    }

    static func tellPrefix(_ inScriptTimeout: Int) -> String {
        """
        with timeout of \(inScriptTimeout) seconds
        tell application id "com.apple.mail"
        """
    }
    static let tellSuffix = """
        end tell
        end timeout
        """

    /// Mailboxes are addressed by INDEX within the account, never by name:
    /// `mailboxList` returns nested (child) mailboxes too, and a name path
    /// like `mailbox "Child" of account "A"` only resolves top-level ones
    /// (verified: -1728 for a nested name that enumeration itself returned).
    /// Index form resolves the same collection the enumeration walked.
    static func mailboxRef(_ mailboxIndex: Int, _ account: String) -> String {
        "mailbox \(mailboxIndex) of account \(quotedAppleString(account))"
    }

    /// " whose read status is false and date sent is less than or equal to
    /// ((current date) - 30 * days)" — date arithmetic stays locale-safe.
    /// Empty string when the selection is unrestricted.
    public static func whoseClause(_ selection: BulkSelection) -> String {
        var terms: [String] = []
        if let unread = selection.unread {
            terms.append("read status is \(unread ? "false" : "true")")
        }
        if let days = selection.olderThanDays {
            terms.append("date sent is less than or equal to ((current date) - \(days) * days)")
        }
        return terms.isEmpty ? "" : " whose " + terms.joined(separator: " and ")
    }

    /// Contiguous index ranges covering one mailbox walk, in walk order,
    /// plus whether the pre-walk probe chunk should be processed first.
    ///
    /// - newest-first: positions 1…min(total, walkLimit) in ascending
    ///   chunks; `plan[0]` IS the probe range, so the probe result is
    ///   processed and the loop starts at plan index 1.
    /// - oldest-first: the walk starts at the far (newest) end; the probe
    ///   chunk overlaps only the plan's tail and is NOT processed here —
    ///   the walk covers those rows when it reaches that chunk. Probe and
    ///   plan overlap is harmless either way: ids dedupe at ingest.
    public static func walkPlan(total: Int, walkLimit: Int, chunkSize: Int, newestFirst: Bool)
        -> (plan: [ClosedRange<Int>], processProbeFirst: Bool) {
        var ranges: [ClosedRange<Int>] = []
        if newestFirst {
            let limit = min(total, walkLimit)
            var s = 1
            while s <= limit {
                let e = min(s + chunkSize - 1, limit)
                ranges.append(s...e)
                s = e + 1
            }
            return (ranges, true)
        }
        let floor = max(1, total - walkLimit + 1)
        var e = total
        while e >= floor {
            let s = max(e - chunkSize + 1, floor)
            ranges.append(s...e)
            e = s - 1
        }
        return (ranges, false)
    }

    /// Decision after a failed chunk read. `refreshedTotal` is a re-run
    /// `countMessages` (nil when that also failed). Ranges beyond a
    /// shrunken mailbox evaporate without counting a transport failure;
    /// everything else retries the SAME range until the strike budget is
    /// out — a transient failure must never silently drop 12 messages.
    public static func failureDecision(range: ClosedRange<Int>, refreshedTotal: Int?,
                                       consecutiveFailures: Int, maxConsecutive: Int = 3) -> FailureDecision {
        if let nt = refreshedTotal, range.lowerBound > nt { return .advance }
        if consecutiveFailures >= maxConsecutive { return .abort }
        return .retrySame
    }

    public enum FailureDecision {
        case retrySame   // re-fetch the same range after backoff
        case advance     // range evaporated (mailbox shrank) — move on
        case abort       // strike budget exhausted — end the walk
    }

    /// newestFirst decision from two dated samples, in walk order.
    /// nil (either date missing, or equal — ambiguous) lets the caller
    /// resample before falling back to the default.
    public static func orderFromSamples(_ first: Date?, _ second: Date?) -> Bool? {
        guard let a = first, let b = second else { return nil }
        if a == b { return nil }
        return a > b
    }

    /// Map a reply descriptor to per-message rows, handling every shape
    /// Mail answers with (all confirmed live on macOS 27):
    /// - bare record: `messages S thru S` (single message) → one row
    /// - list of records: `get properties of messages …` → rows as-is
    /// - list of lists, outer ≠ expectedProps: ROW-major bundle (one list
    ///   per message)
    /// - list of expectedProps lists: PROPERTY-major COLUMNS (each property
    ///   → a list of per-message values) — repacked into rows; the 7×7
    ///   ambiguous case is resolved by type-homogeneity (columns are
    ///   type-uniform, rows are not)
    /// - flat scalar list: property-major flat bundle — repacked into rows
    public static func rowsFromReply(_ reply: NSAppleEventDescriptor, expectedProps: Int) -> [NSAppleEventDescriptor?] {
        if reply.descriptorType != typeListD {
            return [reply]
        }
        let n = Int(reply.numberOfItems)
        guard n > 0, let first = reply.atIndex(1) else { return [] }
        if first.descriptorType == typeRecordD {
            return (1...n).map { reply.atIndex($0) }  // one record per message
        }
        if first.descriptorType == typeListD {
            let innerCounts = (1...n).compactMap { reply.atIndex($0).map { Int($0.numberOfItems) } }
            let uniform = innerCounts.allSatisfy { $0 == innerCounts.first }
            if n == expectedProps, uniform, let inner = innerCounts.first {
                // outer count == expectedProps: columns vs rows is only
                // decidable by shape of contents
                if inner != expectedProps || isTypeHomogeneous(reply.atIndex(1)) {
                    return repackColumns(reply, props: n, count: inner)
                }
            }
            return (1...n).map { reply.atIndex($0) }  // row-major bundle
        }
        // flat property-major scalar bundle: [p1·all, p2·all, …]
        guard expectedProps > 0, n % expectedProps == 0 else { return [] }
        let count = n / expectedProps
        var rows: [NSAppleEventDescriptor?] = []
        for pos in 0..<count {
            let row = NSAppleEventDescriptor.list()
            for p in 0..<expectedProps {
                row.insert(reply.atIndex(p * count + pos + 1) ?? NSAppleEventDescriptor.null(),
                           at: Int(row.numberOfItems) + 1)
            }
            rows.append(row)
        }
        return rows
    }

    /// Build per-message row lists from property columns: column p holds
    /// property p for every message.
    private static func repackColumns(_ reply: NSAppleEventDescriptor, props: Int, count: Int) -> [NSAppleEventDescriptor?] {
        var rows: [NSAppleEventDescriptor?] = []
        for pos in 0..<count {
            let row = NSAppleEventDescriptor.list()
            for p in 1...props {
                let col = reply.atIndex(p)
                row.insert(col?.atIndex(pos + 1) ?? NSAppleEventDescriptor.null(),
                           at: Int(row.numberOfItems) + 1)
            }
            rows.append(row)
        }
        return rows
    }

    private static func isTypeHomogeneous(_ d: NSAppleEventDescriptor?) -> Bool {
        guard let list = d else { return false }
        let n = Int(list.numberOfItems)
        guard n > 1, let first = list.atIndex(1) else { return true }
        let t = first.descriptorType
        for i in 2...n where list.atIndex(i)?.descriptorType != t {
            return false
        }
        return true
    }

    /// Decimal string for an id descriptor. Wide/real ids ('comp', 'doub')
    /// decode FIRST — their stringValue comes back as lossy scientific
    /// notation. ids just need to be non-empty and stable; ingest dedupes
    /// on them.
    public static func intString(_ d: NSAppleEventDescriptor?) -> String? {
        guard let d else { return nil }
        if d.descriptorType == typeCompD {
            // 'comp' is a 64-bit big-endian integer in descriptor data
            let data = d.data
            if data.count == 8 {
                let v = data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> Int64 in
                    Int64(bitPattern: UInt64(bigEndian: raw.loadUnaligned(fromByteOffset: 0, as: UInt64.self)))
                }
                if v != 0 { return String(v) }
            }
        }
        if d.descriptorType == typeDoubleD {
            // same big-endian convention as 'comp'; doubleValue as fallback
            let data = d.data
            if data.count == 8 {
                let bits = data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> UInt64 in
                    raw.loadUnaligned(fromByteOffset: 0, as: UInt64.self)
                }
                let v = Double(bitPattern: UInt64(bigEndian: bits))
                if v != 0 { return String(Int64(v)) }
            }
            let dv = d.doubleValue
            return dv != 0 ? String(Int64(dv)) : nil
        }
        if let s = d.stringValue, !s.isEmpty { return s }
        let i = d.int32Value
        if i != 0 { return String(i) }
        let v = d.doubleValue
        return v != 0 ? String(Int64(v)) : nil
    }

    /// Map one row (record from `get properties`, or bundle row list in
    /// bundleProps order) to a CrawlRecord with the exact field mapping the
    /// JSONL contract pins (docs/protocol.md, bite-index::store::Record).
    /// Returns nil for rows without a usable id.
    public static func recordFromRow(_ row: NSAppleEventDescriptor?, account: String, mailbox: String,
                                     includeContent: Bool) -> CrawlRecord? {
        guard let row else { return nil }
        if row.descriptorType == typeRecordD {
            return recordFromProperties(row, account: account, mailbox: mailbox, includeContent: includeContent)
        }
        func at(_ i: Int) -> NSAppleEventDescriptor? { row.atIndex(i) }
        let mailID = intString(at(1))
        guard let mailID, !mailID.isEmpty, mailID != "0" else { return nil }
        let sent = at(4)?.dateValue.map { Int64($0.timeIntervalSince1970 * 1000) }
        return CrawlRecord(
            app: "mail", id: mailID, account: account, container: mailbox,
            title: at(2)?.stringValue,
            content: includeContent ? at(8)?.stringValue : nil,
            participants: at(3)?.stringValue,
            start_ms: sent, end_ms: nil, updated_ms: sent,
            read: at(5)?.booleanValue,
            flagged: at(6)?.booleanValue,
            junk: at(7)?.booleanValue,
            completed: nil, priority: nil, props: nil
        )
    }

    /// Record-shape mapping ('get properties' rows), keyed by sdef codes.
    static func recordFromProperties(_ row: NSAppleEventDescriptor, account: String, mailbox: String,
                                     includeContent: Bool) -> CrawlRecord? {
        let mailID = intString(row.forKeyword(kwMessageIDD))
        guard let mailID, !mailID.isEmpty, mailID != "0" else { return nil }
        let sent = row.forKeyword(kwDateSentD)?.dateValue.map { Int64($0.timeIntervalSince1970 * 1000) }
        return CrawlRecord(
            app: "mail", id: mailID, account: account, container: mailbox,
            title: row.forKeyword(kwSubjectD)?.stringValue,
            content: includeContent ? row.forKeyword(kwContentD)?.stringValue : nil,
            participants: row.forKeyword(kwSenderD)?.stringValue,
            start_ms: sent, end_ms: nil, updated_ms: sent,
            read: row.forKeyword(kwReadD)?.booleanValue,
            flagged: row.forKeyword(kwFlaggedD)?.booleanValue,
            junk: row.forKeyword(kwJunkD)?.booleanValue,
            completed: nil, priority: nil, props: nil
        )
    }

    // ── target / health ──

    /// Mail's running state — checked WITHOUT Apple Events so probing a
    /// stopped Mail never launches it.
    public static func mailRunning() -> Bool {
        !NSRunningApplication.runningApplications(withBundleIdentifier: "com.apple.mail").isEmpty
    }

    /// True when Mail answers a trivial scripted event within `seconds`.
    public static func healthy(timeoutSeconds: Int32 = 15) -> Bool {
        guard mailRunning() else {
            lastError = "Mail is not running"
            return false
        }
        return run("""
        \(tellPrefix(10))
        count accounts
        \(tellSuffix)
        """, timeoutSeconds: TimeInterval(timeoutSeconds)) != nil
    }

    // ── enumeration one-shots ──

    /// Probe: can this process read basic Mail data? (doctor/diagnostics)
    /// nil = transport failure; Some(0) = Mail answered but has no accounts.
    public static func probeMailAccounts(timeoutSeconds: Int32 = 60) -> Int? {
        countAccounts(timeoutSeconds: timeoutSeconds)
    }

    public static func countAccounts(timeoutSeconds: Int32 = 60) -> Int? {
        guard mailRunning() else { lastError = "Mail is not running"; return nil }
        guard let reply = run("""
        \(tellPrefix(45))
        count accounts
        \(tellSuffix)
        """, timeoutSeconds: TimeInterval(timeoutSeconds)) else { return nil }
        return normalizeCount(reply)
    }

    /// Account names in Mail's own order (index = 1-based position).
    public static func accountList(timeoutSeconds: Int32 = 60) -> [String]? {
        guard mailRunning() else { lastError = "Mail is not running"; return nil }
        guard let reply = run("""
        \(tellPrefix(45))
        get name of every account
        \(tellSuffix)
        """, timeoutSeconds: TimeInterval(timeoutSeconds)) else { return nil }
        var out: [String] = []
        for i in 1...max(reply.numberOfItems, 1) where i <= reply.numberOfItems {
            if let s = reply.atIndex(i)?.stringValue { out.append(s) }
        }
        return out.isEmpty ? nil : out
    }

    /// Mailbox names of one account, in Mail's own order.
    public static func mailboxList(account: String, timeoutSeconds: Int32 = 60) -> [String]? {
        guard mailRunning() else { lastError = "Mail is not running"; return nil }
        let src = """
        \(tellPrefix(45))
        get name of every mailbox of account \(quotedAppleString(account))
        \(tellSuffix)
        """
        guard let reply = run(src, timeoutSeconds: TimeInterval(timeoutSeconds)) else { return nil }
        var out: [String] = []
        for i in 1...max(reply.numberOfItems, 1) where i <= reply.numberOfItems {
            if let s = reply.atIndex(i)?.stringValue { out.append(s) }
        }
        return out.isEmpty ? nil : out
    }

    public static func countMessages(mailboxAt: Int, account: String, timeoutSeconds: Int32 = 360) -> Int? {
        guard mailRunning() else { lastError = "Mail is not running"; return nil }
        let src = """
        \(tellPrefix(240))
        count messages of \(mailboxRef(mailboxAt, account))
        \(tellSuffix)
        """
        guard let reply = run(src, timeoutSeconds: TimeInterval(timeoutSeconds)) else { return nil }
        return normalizeCount(reply)
    }

    // ── message reads ──

    /// One script execution per chunk. With bodies: `get properties …`
    /// (full record, ~2× the cost). Without: an explicit 7-property bundle
    /// so bodies are never fetched. Returns one row descriptor per message
    /// in ascending position order; nil on script error / watchdog overrun
    /// (see `lastError`).
    public static func readProperties(mailboxAt: Int, account: String, start: Int, end: Int,
                                      includeContent: Bool, timeoutSeconds: Int32 = 400) -> [NSAppleEventDescriptor?]? {
        guard mailRunning() else { lastError = "Mail is not running"; return nil }
        let fetch: String
        if includeContent {
            fetch = "get properties of messages \(start) thru \(end) of mb"
        } else {
            fetch = "get {\(bundleProps)} of messages \(start) thru \(end) of mb"
        }
        let src = """
        \(tellPrefix(240))
        set mb to \(mailboxRef(mailboxAt, account))
        \(fetch)
        \(tellSuffix)
        """
        guard let reply = run(src, timeoutSeconds: TimeInterval(timeoutSeconds)) else { return nil }
        let rows = rowsFromReply(reply, expectedProps: includeContent ? 0 : 7)
        return rows.isEmpty ? nil : rows
    }

    // ── bulk primitives (whose-based; one script per operation) ──

    /// `count (every message … whose …)` — bounded by the caller via
    /// timeoutSeconds; bulk counts on the big INBOX may run minutes.
    public static func countWhose(mailboxAt: Int, account: String, selection: BulkSelection,
                                  timeoutSeconds: Int32 = 900) -> Int? {
        guard mailRunning() else { lastError = "Mail is not running"; return nil }
        let src = """
        \(tellPrefix(600))
        count (every message of \(mailboxRef(mailboxAt, account))\(whoseClause(selection)))
        \(tellSuffix)
        """
        guard let reply = run(src, timeoutSeconds: TimeInterval(timeoutSeconds)) else { return nil }
        return normalizeCount(reply)
    }

    /// `set <property> of (every message … whose …) to value` — one script.
    public static func setWhose(mailboxAt: Int, account: String, selection: BulkSelection,
                                property: String, value: Bool, timeoutSeconds: Int32 = 900) -> (ok: Bool, error: String?) {
        guard mailRunning() else { return (false, "Mail is not running") }
        let src = """
        \(tellPrefix(600))
        set \(property) of (every message of \(mailboxRef(mailboxAt, account))\(whoseClause(selection))) to \(value)
        \(tellSuffix)
        """
        return runOk(src, timeoutSeconds: TimeInterval(timeoutSeconds))
    }

    /// `move (every message … whose …) to mailbox …` — one script.
    public static func moveWhose(mailboxAt: Int, account: String, selection: BulkSelection,
                                 toMailboxAt: Int, toAccount: String, timeoutSeconds: Int32 = 900) -> (ok: Bool, error: String?) {
        guard mailRunning() else { return (false, "Mail is not running") }
        let src = """
        \(tellPrefix(600))
        move (every message of \(mailboxRef(mailboxAt, account))\(whoseClause(selection))) to \(mailboxRef(toMailboxAt, toAccount))
        \(tellSuffix)
        """
        return runOk(src, timeoutSeconds: TimeInterval(timeoutSeconds))
    }

    /// `delete (every message … whose …)` — one script.
    public static func deleteWhose(mailboxAt: Int, account: String, selection: BulkSelection,
                                   timeoutSeconds: Int32 = 900) -> (ok: Bool, error: String?) {
        guard mailRunning() else { return (false, "Mail is not running") }
        let src = """
        \(tellPrefix(600))
        delete (every message of \(mailboxRef(mailboxAt, account))\(whoseClause(selection)))
        \(tellSuffix)
        """
        return runOk(src, timeoutSeconds: TimeInterval(timeoutSeconds))
    }

    // ── executor plumbing ──

    static func run(_ source: String, timeoutSeconds: TimeInterval) -> NSAppleEventDescriptor? {
        OSAExecutor.shared.run(source, timeoutSeconds: timeoutSeconds)
    }

    static func runOk(_ source: String, timeoutSeconds: TimeInterval) -> (ok: Bool, error: String?) {
        if run(source, timeoutSeconds: timeoutSeconds) != nil { return (true, nil) }
        return (false, lastError ?? "unknown OSA failure")
    }

    static func normalizeCount(_ reply: NSAppleEventDescriptor) -> Int? {
        let v = reply.int32Value
        return v < 0 ? nil : Int(v)
    }
}
