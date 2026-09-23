// Bulk Mail operations (mark / move / delete) — Mail evaluates each
// whose-clause specifier internally in ONE Apple Event, so 100k messages
// cost the same as one. Mail may take minutes on huge mailboxes; the
// per-AE timeout is raised accordingly and the caller treats this as a
// long-running job (bite-crawl --bulk …).

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
    /// Wait until Mail answers a trivial Apple Event. Returns false when the
    /// patience budget (attempts × interval) runs out.
    public static func waitHealthy(target: NSAppleEventDescriptor, attempts: Int, interval: TimeInterval,
                                   isCancelled: () -> Bool, progress: (String) -> Void) -> Bool {
        for attempt in 1...attempts {
            if isCancelled() { return false }
            if MailAE.healthy(target: target) { return true }
            progress("waiting_mail (attempt \(attempt)/\(attempts))")
            Thread.sleep(forTimeInterval: interval)
        }
        return MailAE.healthy(target: target)
    }

    public static func run(jobID: String, op: String, accountName: String?, mailboxName: String,
                           toMailboxName: String?, selection: BulkSelection,
                           setRead: Bool?, setFlagged: Bool?, setJunk: Bool?,
                           target: NSAppleEventDescriptor, writeState: @escaping (String, Int) -> Void) {
        guard let accounts = MailAE.accountList(target: target) else {
            writeState("failed", 0)
            return
        }
        var source: (account: String, name: String, spec: NSAppleEventDescriptor)?
        for acct in accounts {
            if let accountName, acct.name.caseInsensitiveCompare(accountName) != .orderedSame { continue }
            guard let boxes = MailAE.mailboxList(accountName: acct.name, account: acct.spec, target: target) else { continue }
            if let hit = boxes.first(where: { $0.name.caseInsensitiveCompare(mailboxName) == .orderedSame }) {
                source = (acct.name, hit.name, hit.spec)
                break
            }
        }
        guard let src = source else {
            writeState("failed", 0)
            return
        }

        let selector = MailAE.messageSelector(mailbox: src.spec, selection: selection, now: Date())

        var destSpec: NSAppleEventDescriptor?
        if op == "move" {
            guard let toName = toMailboxName else {
                writeState("failed", 0)
                return
            }
            for acct in accounts {
                guard let boxes = MailAE.mailboxList(accountName: acct.name, account: acct.spec, target: target) else { continue }
                if let hit = boxes.first(where: { $0.name.caseInsensitiveCompare(toName) == .orderedSame }) {
                    destSpec = hit.spec
                    break
                }
            }
            guard destSpec != nil else {
                writeState("failed", 0)
                return
            }
        }

        let estimated = MailAE.countWhose(selector, target: target, timeoutSeconds: 120)
        writeState("running", estimated ?? 0)

        switch op {
        case "mark":
            var failed: String?
            if let read = setRead {
                let (ok, err) = MailAE.setStatus(pReadD, on: selector, to: read, target: target, timeoutSeconds: 900)
                if !ok { failed = err }
            }
            if failed == nil, let flagged = setFlagged {
                let (ok, err) = MailAE.setStatus(pFlaggedD, on: selector, to: flagged, target: target, timeoutSeconds: 900)
                if !ok { failed = err }
            }
            if failed == nil, let junk = setJunk {
                let (ok, err) = MailAE.setStatus(pJunkD, on: selector, to: junk, target: target, timeoutSeconds: 900)
                if !ok { failed = err }
            }
            writeState(failed == nil ? "done" : "failed", estimated ?? 0)

        case "move":
            guard let dest = destSpec else { writeState("failed", 0); return }
            let event = NSAppleEventDescriptor(
                eventClass: 0x636f7265, eventID: 0x6d6f7665,  // 'core'+'move'
                targetDescriptor: target, returnID: AEReturnID(-1), transactionID: 0
            )
            event.setParam(selector, forKeyword: keyDirectObject)
            event.setParam(dest, forKeyword: keyAEInsertHere)
            let (ok, _, err) = MailAE.sendBulk(event, target: target, timeoutSeconds: 900)
            let remaining = ok ? (MailAE.countWhose(selector, target: target, timeoutSeconds: 120) ?? 0) : 0
            writeState(ok && remaining == 0 ? "done" : "running", remaining)
            if !ok { writeState("failed", remaining) }

        case "delete":
            let event = NSAppleEventDescriptor(
                eventClass: 0x636f7265, eventID: 0x64656c6f,  // 'core'+'delo'
                targetDescriptor: target, returnID: AEReturnID(-1), transactionID: 0
            )
            event.setParam(selector, forKeyword: keyDirectObject)
            let (ok, _, err) = MailAE.sendBulk(event, target: target, timeoutSeconds: 900)
            let remaining = ok ? (MailAE.countWhose(selector, target: target, timeoutSeconds: 120) ?? 0) : 0
            writeState(ok ? "done" : "failed", remaining)

        default:
            writeState("failed", 0)
        }
    }
}
