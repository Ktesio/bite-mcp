// Bulk Mail operations (mark / move / delete) — each whose-clause runs as
// ONE scripted command that Mail evaluates internally in a single pass, so
// 100k messages cost the same as one. Mail may take minutes on huge
// mailboxes; the in-script timeout is raised accordingly and the caller
// treats this as a long-running job (bite-crawl --bulk …).

import Foundation

public struct BulkSelection {
    /// select unread messages (read status = false)
    public var unread: Bool?
    /// select older than N days
    public var olderThanDays: Int?

    public init(unread: Bool? = nil, olderThanDays: Int? = nil) {
        self.unread = unread
        self.olderThanDays = olderThanDays
    }

    public var isEmpty: Bool { unread == nil && olderThanDays == nil }
    public var label: String {
        var parts: [String] = []
        if let unread, unread { parts.append("unread") }
        if let olderThanDays { parts.append("olderThanDays=\(olderThanDays)") }
        return parts.isEmpty ? "all messages" : parts.joined(separator: " AND ")
    }
}

public enum MailBulk {
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
        guard let accounts = MailAE.accountList() else {
            writeState("failed", 0)
            return
        }
        func resolve(_ name: String, within: [(account: String, name: String)]) -> (account: String, name: String)? {
            for box in within where box.name.caseInsensitiveCompare(name) == .orderedSame {
                return box
            }
            return nil
        }
        var all: [(account: String, name: String)] = []
        for acct in accounts {
            guard let boxes = MailAE.mailboxList(account: acct) else { continue }
            for b in boxes { all.append((acct, b)) }
        }
        // Source respects the account filter; move destinations search every
        // account (first match wins), matching the previous behavior.
        let sourceable: [(account: String, name: String)]
        if let accountName {
            sourceable = all.filter { $0.account.caseInsensitiveCompare(accountName) == .orderedSame }
        } else {
            sourceable = all
        }
        guard let src = resolve(mailboxName, within: sourceable) else {
            writeState("failed", 0)
            return
        }

        var dest: (account: String, name: String)?
        if op == "move" {
            guard let toName = toMailboxName else {
                writeState("failed", 0)
                return
            }
            dest = resolve(toName, within: all)
            guard dest != nil else {
                writeState("failed", 0)
                return
            }
        }

        let estimated = MailAE.countWhose(mailbox: src.name, account: src.account, selection: selection)
        writeState("running", estimated ?? 0)

        switch op {
        case "mark":
            var failed: String?
            if let read = setRead {
                let (ok, err) = MailAE.setWhose(mailbox: src.name, account: src.account, selection: selection,
                                                property: "read status", value: read)
                if !ok { failed = err }
            }
            if failed == nil, let flagged = setFlagged {
                let (ok, err) = MailAE.setWhose(mailbox: src.name, account: src.account, selection: selection,
                                                property: "flagged status", value: flagged)
                if !ok { failed = err }
            }
            if failed == nil, let junk = setJunk {
                let (ok, err) = MailAE.setWhose(mailbox: src.name, account: src.account, selection: selection,
                                                property: "junk mail status", value: junk)
                if !ok { failed = err }
            }
            writeState(failed == nil ? "done" : "failed", estimated ?? 0)

        case "move":
            guard let dest else { writeState("failed", 0); return }
            let (ok, err) = MailAE.moveWhose(mailbox: src.name, account: src.account, selection: selection,
                                             toMailbox: dest.name, toAccount: dest.account)
            _ = err
            let remaining = ok ? (MailAE.countWhose(mailbox: src.name, account: src.account, selection: selection) ?? 0) : 0
            writeState(ok && remaining == 0 ? "done" : "running", remaining)
            if !ok { writeState("failed", remaining) }

        case "delete":
            let (ok, _) = MailAE.deleteWhose(mailbox: src.name, account: src.account, selection: selection)
            let remaining = ok ? (MailAE.countWhose(mailbox: src.name, account: src.account, selection: selection) ?? 0) : 0
            writeState(ok ? "done" : "failed", remaining)

        default:
            writeState("failed", 0)
        }
    }
}
