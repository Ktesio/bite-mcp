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
// - Message reads use `get properties of messages S thru E` — ONE bundled
//   get per message, evaluated store-side. Loops that fetch properties in
//   separate accesses wedge Mail's AE queue (gate-1 finding), and per-
//   property bundles measured ~5 s/property while a range-of-properties
//   fetch costs ~3-5 s per message for the FULL record.
// - Heavy operations (counts, whose-based bulk ops) raise the in-script
//   `with timeout` — Mail's default 120 s AppleEvent timeout is exceeded by
//   bulk work on the big INBOX.
// - Mail's mailboxes are ACCOUNT-scoped: always account → mailbox →
//   messages.

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
            // instead of a deadlock.
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

    // ── script text builders (pure; unit-tested) ──

    /// AppleScript string literal with backslash and quote escaped.
    public static func quotedAppleString(_ s: String) -> String {
        "\"" + s
            .replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"") + "\""
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

    static func mailboxRef(_ mailbox: String, _ account: String) -> String {
        "mailbox \(quotedAppleString(mailbox)) of account \(quotedAppleString(account))"
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

    /// Contiguous index ranges covering one mailbox walk, in walk order.
    /// Newest-first covers positions 1…min(total, walkLimit); oldest-first
    /// covers the same-sized slice at the far end, walked downward.
    public static func chunkPlan(total: Int, walkLimit: Int, chunkSize: Int, newestFirst: Bool) -> [(start: Int, end: Int)] {
        var out: [(start: Int, end: Int)] = []
        if newestFirst {
            let limit = min(total, walkLimit)
            var s = 1
            while s <= limit {
                let e = min(s + chunkSize - 1, limit)
                out.append((s, e))
                s = e + 1
            }
        } else {
            let floor = max(1, total - walkLimit + 1)
            var e = total
            while e >= floor {
                let s = max(e - chunkSize + 1, floor)
                out.append((s, e))
                e = s - 1
            }
        }
        return out
    }

    /// Map one `properties` record descriptor to a CrawlRecord using the
    /// exact field mapping the JSONL contract pins (docs/protocol.md,
    /// bite-index::store::Record). Returns nil for rows without a usable id.
    public static func recordFromProperties(_ row: NSAppleEventDescriptor?, account: String, mailbox: String,
                                            includeContent: Bool) -> CrawlRecord? {
        guard let row else { return nil }
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

    /// Decimal string for an id descriptor: stringValue coerces both 'long'
    /// and wide-integer ids; int32 is the fallback, double the last resort
    /// (AppleScript widens overflowing integers to real).
    public static func intString(_ d: NSAppleEventDescriptor?) -> String? {
        guard let d else { return nil }
        if let s = d.stringValue, !s.isEmpty { return s }
        let i = d.int32Value
        if i != 0 { return String(i) }
        let v = d.doubleValue
        return v != 0 ? String(Int64(v)) : nil
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
        return run("tell application id \"com.apple.mail\" to count accounts",
                   timeoutSeconds: TimeInterval(timeoutSeconds)) != nil
    }

    // ── enumeration one-shots ──

    /// Probe: can this process read basic Mail data? (doctor/diagnostics)
    public static func probeMailAccounts(timeoutSeconds: Int32 = 15) -> Int? {
        countAccounts(timeoutSeconds: timeoutSeconds)
    }

    public static func countAccounts(timeoutSeconds: Int32 = 60) -> Int? {
        guard let reply = run("tell application id \"com.apple.mail\" to count accounts",
                              timeoutSeconds: TimeInterval(timeoutSeconds)) else { return nil }
        return normalizeCount(reply)
    }

    /// Account names in Mail's own order (index = 1-based position).
    public static func accountList(timeoutSeconds: Int32 = 60) -> [String]? {
        guard let reply = run("tell application id \"com.apple.mail\" to get name of every account",
                              timeoutSeconds: TimeInterval(timeoutSeconds)) else { return nil }
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
        \(tellPrefix(120))
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

    public static func countMessages(mailbox: String, account: String, timeoutSeconds: Int32 = 360) -> Int? {
        guard mailRunning() else { lastError = "Mail is not running"; return nil }
        let src = """
        \(tellPrefix(240))
        count messages of \(mailboxRef(mailbox, account))
        \(tellSuffix)
        """
        guard let reply = run(src, timeoutSeconds: TimeInterval(timeoutSeconds)) else { return nil }
        return normalizeCount(reply)
    }

    // ── message reads ──

    /// `get properties of messages S thru E` — ONE script execution whose
    /// single Apple Event fetches the full record of every message in the
    /// range. Returns one record descriptor per message, ascending position.
    /// nil on script error / watchdog overrun (see `lastError`).
    public static func readProperties(mailbox: String, account: String, start: Int, end: Int,
                                      includeContent: Bool, timeoutSeconds: Int32 = 400) -> [NSAppleEventDescriptor?]? {
        guard mailRunning() else { lastError = "Mail is not running"; return nil }
        let src = """
        \(tellPrefix(240))
        set mb to \(mailboxRef(mailbox, account))
        get properties of messages \(start) thru \(end) of mb
        \(tellSuffix)
        """
        guard let reply = run(src, timeoutSeconds: TimeInterval(timeoutSeconds)) else { return nil }
        var out: [NSAppleEventDescriptor?] = []
        for i in 1...max(reply.numberOfItems, 1) where i <= reply.numberOfItems {
            out.append(reply.atIndex(i))
        }
        return out.isEmpty ? nil : out
    }

    // ── bulk primitives (whose-based; one script per operation) ──

    /// `count (every message … whose …)` — bounded by the caller via
    /// timeoutSeconds; bulk counts on the big INBOX may run minutes.
    public static func countWhose(mailbox: String, account: String, selection: BulkSelection,
                                  timeoutSeconds: Int32 = 900) -> Int? {
        guard mailRunning() else { lastError = "Mail is not running"; return nil }
        let src = """
        \(tellPrefix(600))
        count (every message of \(mailboxRef(mailbox, account))\(whoseClause(selection)))
        \(tellSuffix)
        """
        guard let reply = run(src, timeoutSeconds: TimeInterval(timeoutSeconds)) else { return nil }
        return normalizeCount(reply)
    }

    /// `set <property> of (every message … whose …) to value` — one script.
    public static func setWhose(mailbox: String, account: String, selection: BulkSelection,
                                property: String, value: Bool, timeoutSeconds: Int32 = 900) -> (ok: Bool, error: String?) {
        guard mailRunning() else { return (false, "Mail is not running") }
        let src = """
        \(tellPrefix(600))
        set \(property) of (every message of \(mailboxRef(mailbox, account))\(whoseClause(selection))) to \(value)
        \(tellSuffix)
        """
        return runOk(src, timeoutSeconds: TimeInterval(timeoutSeconds))
    }

    /// `move (every message … whose …) to mailbox …` — one script.
    public static func moveWhose(mailbox: String, account: String, selection: BulkSelection,
                                 toMailbox: String, toAccount: String, timeoutSeconds: Int32 = 900) -> (ok: Bool, error: String?) {
        guard mailRunning() else { return (false, "Mail is not running") }
        let src = """
        \(tellPrefix(600))
        move (every message of \(mailboxRef(mailbox, account))\(whoseClause(selection))) to \(mailboxRef(toMailbox, toAccount))
        \(tellSuffix)
        """
        return runOk(src, timeoutSeconds: TimeInterval(timeoutSeconds))
    }

    /// `delete (every message … whose …)` — one script.
    public static func deleteWhose(mailbox: String, account: String, selection: BulkSelection,
                                   timeoutSeconds: Int32 = 900) -> (ok: Bool, error: String?) {
        guard mailRunning() else { return (false, "Mail is not running") }
        let src = """
        \(tellPrefix(600))
        delete (every message of \(mailboxRef(mailbox, account))\(whoseClause(selection)))
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
