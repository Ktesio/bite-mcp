// Bulk Mail operations (mark / move / delete) — each whose-clause runs as
// ONE scripted command that Mail evaluates internally in a single pass, so
// 100k messages cost the same as one. Mail may take minutes on huge
// mailboxes; the in-script timeout is raised accordingly and the caller
// treats this as a long-running job (bite-crawl --bulk …).
//
// Safety rails: unbounded selections are refused (a bulk op must be
// scoped), mark needs at least one target property, and moves refuse to
// target the source mailbox. Confirm flows live on the Rust side.

import Foundation

public struct BulkSelection {
    /// select messages by read status: true → unread messages,
    /// false → already-read messages
    public var unread: Bool?
    /// select older than N days (always > 0; the CLI and the Rust spawner
    /// both reject non-positive values)
    public var olderThanDays: Int?

    public init(unread: Bool? = nil, olderThanDays: Int? = nil) {
        self.unread = unread
        self.olderThanDays = olderThanDays
    }

    public var isEmpty: Bool { unread == nil && olderThanDays == nil }
    public var label: String {
        var parts: [String] = []
        if let unread { parts.append(unread ? "unread" : "read") }
        if let olderThanDays { parts.append("olderThanDays=\(olderThanDays)") }
        return parts.isEmpty ? "all messages" : parts.joined(separator: " AND ")
    }
}

public enum MailBulk {
    /// Terminal-state table for one executed bulk operation:
    /// ok → the operation SUCCEEDED — done when the verification count
    /// agrees (remaining 0) or notes "partial" (remaining > 0); a
    /// verification count that TIMES OUT (nil) must not fail a succeeded
    /// destructive op — it reports done and the caller notes the unknown
    /// remaining. Only an operation error → "failed".
    public static func terminalState(ok: Bool, remaining: Int?) -> String {
        guard ok else { return "failed" }
        guard let remaining else { return "done" }
        return remaining == 0 ? "done" : "partial"
    }

    /// Synchronous bulk run used by `bite-crawl --bulk-worker`.
    public static func runBulkWorker(op: String, accountName: String?, mailboxName: String,
                                     toMailboxName: String?, selection: BulkSelection,
                                     setRead: Bool?, setFlagged: Bool?, setJunk: Bool?,
                                     writeState: @escaping (String, Int) -> Void) {
        run(op: op, accountName: accountName, mailboxName: mailboxName,
            toMailboxName: toMailboxName, selection: selection,
            setRead: setRead, setFlagged: setFlagged, setJunk: setJunk,
            writeState: writeState)
    }

    /// Wait until Mail answers a trivial scripted event. Returns false when
    /// the patience budget (attempts × interval) runs out.
    public static func waitHealthy(attempts: Int, interval: TimeInterval,
                                   isCancelled: () -> Bool, progress: (String) -> Void) -> Bool {
        for attempt in 1...attempts {
            if isCancelled() { return false }
            if MailAE.healthy() { return true }
            progress("waiting_mail (attempt \(attempt)/\(attempts))")
            Thread.sleep(forTimeInterval: interval)
        }
        return MailAE.healthy()
    }

    static func run(op: String, accountName: String?, mailboxName: String,
                    toMailboxName: String?, selection: BulkSelection,
                    setRead: Bool?, setFlagged: Bool?, setJunk: Bool?,
                    writeState: @escaping (String, Int) -> Void) {
        guard MailAE.mailRunning() else {
            lastError = "Mail is not running"
            writeState("failed", 0)
            return
        }
        // an unscoped bulk op would touch the whole mailbox in one event —
        // require an explicit selection
        guard !selection.isEmpty else {
            lastError = "refusing unbounded bulk \(op): pass --unread <true|false> and/or --older-than-days <N> to scope it"
            writeState("failed", 0)
            return
        }
        if op == "mark", setRead == nil, setFlagged == nil, setJunk == nil {
            lastError = "bulk mark needs at least one of --set-read/--set-flagged/--set-junk"
            writeState("failed", 0)
            return
        }

        guard let accounts = MailAE.accountList() else {
            writeState("failed", 0)
            return
        }
        // Mailboxes are addressed by index within the account (names alone
        // don't resolve for nested/child mailboxes); names ride along for
        // labels and record fields.
        func resolve(_ name: String, within: [(account: String, index: Int, name: String)])
            -> (account: String, index: Int, name: String)? {
            for box in within where box.name.caseInsensitiveCompare(name) == .orderedSame {
                return box
            }
            return nil
        }
        var all: [(account: String, index: Int, name: String)] = []
        for acct in accounts {
            guard let boxes = MailAE.mailboxList(account: acct) else { continue }
            for (idx, b) in boxes.enumerated() { all.append((acct, idx + 1, b)) }
        }
        // Source respects the account filter; move destinations search every
        // account (first match wins), matching the historical behavior.
        let sourceable: [(account: String, index: Int, name: String)]
        if let accountName {
            sourceable = all.filter { $0.account.caseInsensitiveCompare(accountName) == .orderedSame }
        } else {
            sourceable = all
        }
        guard let src = resolve(mailboxName, within: sourceable) else {
            writeState("failed", 0)
            return
        }

        var dest: (account: String, index: Int, name: String)?
        if op == "move" {
            guard let toName = toMailboxName else {
                writeState("failed", 0)
                return
            }
            dest = resolve(toName, within: all)
            guard let dest else {
                writeState("failed", 0)
                return
            }
            // moving a mailbox onto itself would be a destructive no-op
            if dest.account.caseInsensitiveCompare(src.account) == .orderedSame,
               dest.name.caseInsensitiveCompare(src.name) == .orderedSame {
                lastError = "move refused: '\(dest.name)' in '\(dest.account)' is the source mailbox"
                writeState("failed", 0)
                return
            }
        }

        // identity guard before any operation touches an index-resolved
        // mailbox — a shifted enumeration order would misdirect the op.
        // Returns false (state already written) on failure. `context` is
        // appended to the failure so a mid-group refusal records what the
        // operation ALREADY applied (an operator re-run would otherwise
        // silently diverge: the selection no longer matches).
        func identityFailure(_ box: (account: String, index: Int, name: String)) -> String? {
            if let current = MailAE.mailboxName(at: box.index, account: box.account),
               !current.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                if current.caseInsensitiveCompare(box.name) == .orderedSame { return nil }
                return "mailbox \(box.index) in '\(box.account)' now resolves to '\(current)', expected '\(box.name)' — refusing to run bulk \(op) against a moved target"
            }
            return "couldn't verify identity of mailbox \(box.index) in '\(box.account)' before bulk \(op) — \(lastError ?? "unknown error")"
        }
        // A nil/empty name is transient (executor busy / watchdog / null
        // reply): retry twice with a pause before failing the op closed. A
        // definitive name mismatch still fails immediately.
        func requireIdentity(_ box: (account: String, index: Int, name: String), estimated: Int?, context: String? = nil) -> Bool {
            for attempt in 0...2 {
                if attempt > 0 { Thread.sleep(forTimeInterval: 3) }
                if let failure = identityFailure(box) {
                    lastError = context.map { "\(failure) — \($0)" } ?? failure
                    if failure.contains("now resolves to") {
                        writeState("failed", estimated ?? 0)
                        return false  // definitive mismatch — no retry
                    }
                    continue  // transient — retry
                }
                return true
            }
            writeState("failed", estimated ?? 0)
            return false
        }
        guard requireIdentity(src, estimated: nil) else { return }
        let estimated = MailAE.countWhose(mailboxAt: src.index, account: src.account, selection: selection)
        writeState("running", estimated ?? 0)

        switch op {
        case "mark":
            guard requireIdentity(src, estimated: estimated) else { return }
            var failed: String?
            var applied: [String] = []
            // what earlier setters already changed — surfaced when a later
            // identity check refuses, so an operator re-run knows the
            // selection may no longer match
            func appliedContext() -> String? {
                applied.isEmpty ? nil
                    : "partially applied: \(applied.joined(separator: ", ")) — re-running with the same selection may not match"
            }
            if let read = setRead {
                guard requireIdentity(src, estimated: estimated, context: appliedContext()) else { return }
                let (ok, err) = MailAE.setWhose(mailboxAt: src.index, account: src.account, selection: selection,
                                                property: "read status", value: read)
                if !ok { failed = err } else { applied.append("read=\(read)") }
            }
            if failed == nil, let flagged = setFlagged {
                // re-validate before EACH setter: a mid-group enumeration
                // shift would misdirect setters 2/3
                guard requireIdentity(src, estimated: estimated, context: appliedContext()) else { return }
                let (ok, err) = MailAE.setWhose(mailboxAt: src.index, account: src.account, selection: selection,
                                                property: "flagged status", value: flagged)
                if !ok { failed = err } else { applied.append("flagged=\(flagged)") }
            }
            if failed == nil, let junk = setJunk {
                guard requireIdentity(src, estimated: estimated, context: appliedContext()) else { return }
                let (ok, err) = MailAE.setWhose(mailboxAt: src.index, account: src.account, selection: selection,
                                                property: "junk mail status", value: junk)
                if !ok { failed = err } else { applied.append("junk=\(junk)") }
            }
            writeState(failed == nil ? "done" : "failed", estimated ?? 0)

        case "move":
            guard let dest else { writeState("failed", 0); return }
            // re-validate BOTH ends immediately before the operation: the
            // initial check can be ~minutes stale by the time Mail executes
            guard requireIdentity(src, estimated: estimated) else { return }
            guard requireIdentity(dest, estimated: estimated) else { return }
            let (ok, _) = MailAE.moveWhose(mailboxAt: src.index, account: src.account, selection: selection,
                                           toMailboxAt: dest.index, toAccount: dest.account)
            let remaining = ok ? MailAE.countWhose(mailboxAt: src.index, account: src.account, selection: selection) : nil
            if ok, remaining == nil {
                // succeeded, but the verification count timed out — don't
                // hide that from the operator
                CrawlState.shared.setWindow("bulk move done — verification count timed out, remaining unknown")
            }
            writeState(terminalState(ok: ok, remaining: remaining), remaining ?? 0)

        case "delete":
            guard requireIdentity(src, estimated: estimated) else { return }
            let (ok, _) = MailAE.deleteWhose(mailboxAt: src.index, account: src.account, selection: selection)
            let remaining = ok ? MailAE.countWhose(mailboxAt: src.index, account: src.account, selection: selection) : nil
            if ok, remaining == nil {
                CrawlState.shared.setWindow("bulk delete done — verification count timed out, remaining unknown")
            }
            writeState(terminalState(ok: ok, remaining: remaining), remaining ?? 0)

        default:
            writeState("failed", 0)
        }
    }
}
