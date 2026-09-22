import Foundation
import ScriptingBridge

/// Apple Mail via dynamic ScriptingBridge.
///
/// Mail exposes no native framework; ScriptingBridge (Apple Events) is the
/// sanctioned path. Two consequences shape this module:
/// - Message enumeration costs one Apple Event per message, so searches walk
///   newest→oldest with a scan cap and report `truncated` honestly.
/// - Follow-up ops (get/reply/move/…) require the `mailbox` that a search
///   result carried, so the message is located in one mailbox instead of every
///   mailbox in every account.

let MAIL_BUNDLE = "com.apple.mail"

private func mailApp() throws -> SBApplication {
    try sbApp(bundleID: MAIL_BUNDLE, name: "Mail")
}

enum MailBridge {

    // MARK: lookup helpers

    static func accounts(_ app: SBApplication) throws -> SBElementArray {
        guard let accounts = sbElements(app, "accounts"), accounts.count > 0 else {
            throw BridgeError(code: "app_not_running", message: "Mail has no configured accounts", app: "Mail")
        }
        return accounts
    }

    /// Resolve a mailbox by name across accounts (or within one account).
    static func findMailbox(_ app: SBApplication, account: String?, name: String?) throws -> (SBObject, String) {
        let accounts = try accounts(app)
        var fallback: (SBObject, String)?
        for i in 1...accounts.count {
            guard let acct = sbAt(accounts, i) else { continue }
            let acctName = sbStr(acct, "name")
            if let account, acctName.caseInsensitiveCompare(account) != .orderedSame { continue }
            guard let boxes = sbElements(acct, "mailboxes") else { continue }
            for j in 1...boxes.count {
                guard let box = sbAt(boxes, j) else { continue }
                let boxName = sbStr(box, "name")
                if let name {
                    if boxName.caseInsensitiveCompare(name) == .orderedSame { return (box, boxName) }
                } else if fallback == nil {
                    fallback = (box, boxName)
                }
            }
        }
        if let name {
            throw BridgeError.notFound("No mailbox named '\(name)'" + (account.map { " in account '\($0)'" } ?? ""), app: "Mail")
        }
        guard let first = fallback else {
            throw BridgeError.notFound("No mailboxes found", app: "Mail")
        }
        return first
    }

    /// Locate a message by id. `mailbox` scopes the scan (recommended); without
    /// it we scan the newest 100 messages of every mailbox, capped.
    static func findMessage(_ app: SBApplication, id: String, mailbox: String?, account: String?) throws -> SBObject {
        let cap = 1000
        var targets: [(SBObject, String)] = []
        if let mailbox {
            targets = [try findMailbox(app, account: account, name: mailbox)]
        } else {
            let accounts = try accounts(app)
            for i in 1...accounts.count {
                guard let acct = sbAt(accounts, i), let boxes = sbElements(acct, "mailboxes") else { continue }
                for j in 1...boxes.count {
                    guard let box = sbAt(boxes, j) else { continue }
                    targets.append((box, sbStr(box, "name")))
                }
            }
        }
        let perBoxCap = mailbox == nil ? 100 : cap
        for (box, _) in targets {
            guard let messages = sbElements(box, "messages") else { continue }
            var k = messages.count
            var scanned = 0
            while k >= 1, scanned < perBoxCap {
                if let msg = sbAt(messages, k) {
                    scanned += 1
                    if String(describing: sbGet(msg, "id") ?? "") == id { return msg }
                }
                k -= 1
            }
        }
        throw BridgeError.notFound(
            "No message with id \(id). Tip: pass the `mailbox` from the search result to look only there.",
            app: "Mail"
        )
    }

    // MARK: shaping

    static func summary(_ msg: SBObject, mailboxName: String) -> [String: Any] {
        var d: [String: Any] = [
            "id": String(describing: sbGet(msg, "id") ?? ""),
            "mailbox": mailboxName,
            "subject": sbStr(msg, "subject"),
            "from": sbStr(msg, "sender"),
            "date": D.format(sbDate(msg, "dateSent")) ?? "",
            "read": sbBool(msg, "readStatus") ?? false,
            "flagged": sbBool(msg, "flaggedStatus") ?? false,
        ]
        if let snippet = snippet(sbStr(msg, "content")) { d["snippet"] = snippet }
        let to = recipients(msg, "toRecipients")
        if !to.isEmpty { d["to"] = to }
        return d
    }

    static func recipients(_ msg: SBObject, _ key: String) -> [String] {
        guard let arr = sbElements(msg, key), arr.count > 0 else { return [] }
        var out: [String] = []
        for i in 1...arr.count {
            guard let r = sbAt(arr, i), let addr = sbGet(r, "address") as? String, !addr.isEmpty else { continue }
            out.append(addr)
        }
        return out
    }

    /// First ~200 chars of plain text from message content.
    static func snippet(_ content: String) -> String? {
        let plain = htmlToText(content)
        let trimmed = plain.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }
        if trimmed.count <= 200 { return trimmed }
        return String(trimmed.prefix(200)) + "…"
    }

    /// "On <date>, <sender> wrote:" block quote.
    static func quote(_ msg: SBObject) -> String {
        let date = D.format(sbDate(msg, "dateSent")) ?? ""
        let sender = sbStr(msg, "sender")
        let body = sbStr(msg, "content")
        let header = "\n\nOn \(date), \(sender) wrote:\n"
        let quoted = body.split(separator: "\n").map { "&gt; " + $0 }.joined(separator: "\n<br>\n")
        return header + quoted
    }

    // MARK: send

    static func send(_ app: SBApplication, _ req: Incoming, subject: String,
                     extraTo: [String] = [], cc: [String] = [], prependBody: String = "") throws -> [String: Any] {
        let to = extraTo + J.optStrArray(req.params, "to")
        let ccAll = cc + J.optStrArray(req.params, "cc")
        let bcc = J.optStrArray(req.params, "bcc")
        guard !to.isEmpty || !ccAll.isEmpty || !bcc.isEmpty else {
            throw BridgeError.invalidParams("no recipients: pass 'to' (or 'cc'/'bcc')")
        }
        let bodyIn = J.optStr(req.params, "body") ?? ""
        let asHtml = J.optBool(req.params, "html") ?? false
        let body = asHtml ? bodyIn : markdownToHtml(bodyIn)
        let content = prependBody + "\n" + body

        let props: [String: Any] = ["subject": subject, "content": content, "visible": false]
        let msg = try sbMake(app, className: "outgoing message", properties: props)

        if let sender = J.optStr(req.params, "account") { msg.setValue(sender, forKey: "sender") }

        func addRecipients(_ elementKey: String, _ className: String, _ addresses: [String]) throws {
            guard !addresses.isEmpty, let arr = sbElements(msg, elementKey) else { return }
            for a in addresses where !a.isEmpty {
                arr.add(try sbMake(app, className: className, properties: ["address": a]))
            }
        }
        try addRecipients("toRecipients", "to recipient", to)
        try addRecipients("ccRecipients", "cc recipient", ccAll)
        try addRecipients("bccRecipients", "bcc recipient", bcc)

        for path in J.optStrArray(req.params, "attachments") {
            guard FileManager.default.fileExists(atPath: path) else {
                throw BridgeError.notFound("attachment not found: \(path)", app: "Mail")
            }
            let att = try sbMake(app, className: "attachment", properties: ["fileName": path])
            if let arr = sbElements(msg, "mailAttachments") { arr.add(att) }
        }

        try sbSend(app, selectors: ["send:"], args: [msg])
        return ["sent": true, "subject": subject, "to": to.filter { !$0.isEmpty }]
    }

    // MARK: registration

    static func register(_ d: Dispatcher) {
        d.register("mail.accounts") { req in
            try withWatchdog(30) {
                let app = try mailApp()
                let accounts = try self.accounts(app)
                var out: [[String: Any]] = []
                for i in 1...accounts.count {
                    guard let acct = sbAt(accounts, i) else { continue }
                    let emails = (sbGet(acct, "emailAddresses") as? [Any])?.compactMap { $0 as? String } ?? []
                    out.append([
                        "name": sbStr(acct, "name"),
                        "email": emails.first ?? "",
                    ])
                }
                return ["accounts": out]
            }
        }

        d.register("mail.mailboxes") { req in
            try withWatchdog(60) {
                let app = try mailApp()
                let accountFilter = J.optStr(req.params, "account")
                let accounts = try self.accounts(app)
                var out: [[String: Any]] = []
                for i in 1...accounts.count {
                    guard let acct = sbAt(accounts, i) else { continue }
                    let acctName = sbStr(acct, "name")
                    if let accountFilter, acctName.caseInsensitiveCompare(accountFilter) != .orderedSame { continue }
                    guard let boxes = sbElements(acct, "mailboxes") else { continue }
                    for j in 1...boxes.count {
                        guard let box = sbAt(boxes, j) else { continue }
                        out.append([
                            "name": sbStr(box, "name"),
                            "account": acctName,
                            "unread": (sbGet(box, "unreadCount") as? NSNumber)?.intValue ?? 0,
                        ])
                    }
                }
                return ["mailboxes": out]
            }
        }

        d.register("mail.messages_search") { req in
            try withWatchdog(90) {
                let app = try mailApp()
                let mailboxName = J.optStr(req.params, "mailbox") ?? "INBOX"
                let (box, resolved) = try findMailbox(app, account: J.optStr(req.params, "account"), name: mailboxName)
                guard let messages = sbElements(box, "messages") else {
                    throw BridgeError.notFound("Cannot read messages of '\(resolved)'", app: "Mail")
                }

                let wantFrom = J.optStr(req.params, "from")?.lowercased()
                let wantTo = J.optStr(req.params, "to")?.lowercased()
                let wantSubject = J.optStr(req.params, "subject")?.lowercased()
                let wantBody = J.optStr(req.params, "body")?.lowercased()
                let unread = J.optBool(req.params, "unread")
                let flagged = J.optBool(req.params, "flagged")
                let since = try D.optDate(req.params, "since")
                let until = try D.optDate(req.params, "until")
                let limit = J.optInt(req.params, "limit") ?? 20
                let maxScan = J.optInt(req.params, "max_scan") ?? 400

                var out: [[String: Any]] = []
                var scanned = 0
                var exhausted = false
                let total = messages.count

                // Mailbox ordering is not guaranteed (iCloud INBOX is
                // newest-first, most local mailboxes oldest-first). Detect it
                // by sampling the two ends, then walk newest → oldest.
                let firstDate = sbAt(messages, 1).flatMap { sbDate($0, "dateSent") } ?? .distantPast
                let lastDate = total > 1 ? (sbAt(messages, total).flatMap { sbDate($0, "dateSent") } ?? .distantPast) : firstDate
                let newestFirst = firstDate >= lastDate
                var steps = 0
                while steps < total, out.count < limit, scanned < maxScan {
                    steps += 1
                    let position = newestFirst ? steps : (total - steps + 1)
                    guard let msg = sbAt(messages, position) else { continue }
                    scanned += 1
                    let date = sbDate(msg, "dateSent") ?? .distantPast
                    if newestFirst {
                        if let since, date < since { exhausted = true; break }
                        if let until, date > until { continue }
                    } else {
                        if let until, date > until { continue }
                        if let since, date < since { exhausted = true; break }
                    }
                    if let unread, (sbBool(msg, "readStatus") ?? true) == unread { continue }
                    if let flagged, (sbBool(msg, "flaggedStatus") ?? false) != flagged { continue }
                    if let wantFrom, !sbStr(msg, "sender").lowercased().contains(wantFrom) { continue }
                    if let wantSubject, !sbStr(msg, "subject").lowercased().contains(wantSubject) { continue }
                    if let wantTo, !recipients(msg, "toRecipients").joined(separator: " ").lowercased().contains(wantTo) { continue }
                    if let wantBody, !sbStr(msg, "content").lowercased().contains(wantBody) { continue }
                    out.append(summary(msg, mailboxName: resolved))
                }
                let truncated = !exhausted && (steps < total || scanned >= maxScan)
                return [
                    "messages": out,
                    "scanned": scanned,
                    "truncated": truncated,
                    "count": total,
                    "order": newestFirst ? "newest_first" : "oldest_first",
                ]
            }
        }

        d.register("mail.message_get") { req in
            try withWatchdog(30) {
                let app = try mailApp()
                let id = try J.reqStr(req.params, "id")
                let msg = try findMessage(app, id: id, mailbox: J.optStr(req.params, "mailbox"), account: J.optStr(req.params, "account"))
                var d = summary(msg, mailboxName: J.optStr(req.params, "mailbox") ?? "")
                d["content"] = sbStr(msg, "content")
                if let received = sbDate(msg, "dateReceived") { d["date_received"] = D.format(received) ?? "" }
                var attachments: [[String: Any]] = []
                if let atts = sbElements(msg, "mailAttachments"), atts.count > 0 {
                    for i in 1...atts.count {
                        guard let a = sbAt(atts, i) else { continue }
                        attachments.append(["name": sbStr(a, "name"), "index": i - 1])
                    }
                }
                if !attachments.isEmpty { d["attachments"] = attachments }
                return ["message": d]
            }
        }

        d.register("mail.send") { req in
            try withWatchdog(60) {
                try send(try mailApp(), req, subject: try J.reqStr(req.params, "subject"))
            }
        }

        d.register("mail.reply") { req in
            try withWatchdog(60) {
                let app = try mailApp()
                let id = try J.reqStr(req.params, "id")
                let original = try findMessage(app, id: id, mailbox: J.optStr(req.params, "mailbox"), account: J.optStr(req.params, "account"))
                var to = [sbStr(original, "sender")]
                var cc: [String] = []
                if J.optBool(req.params, "reply_all") == true {
                    cc = recipients(original, "ccRecipients")
                    to += recipients(original, "toRecipients").filter { $0.caseInsensitiveCompare(sbStr(original, "sender")) != .orderedSame }
                }
                let subject = sbStr(original, "subject")
                let subj = subject.lowercased().hasPrefix("re:") ? subject : "Re: " + subject
                return try send(app, req, subject: subj, extraTo: to, cc: cc, prependBody: quote(original))
            }
        }

        d.register("mail.forward") { req in
            try withWatchdog(60) {
                let app = try mailApp()
                let id = try J.reqStr(req.params, "id")
                let original = try findMessage(app, id: id, mailbox: J.optStr(req.params, "mailbox"), account: J.optStr(req.params, "account"))
                let subject = sbStr(original, "subject")
                let subj = subject.lowercased().hasPrefix("fwd:") ? subject : "Fwd: " + subject
                return try send(app, req, subject: subj, prependBody: quote(original))
            }
        }

        d.register("mail.move") { req in
            try withWatchdog(60) {
                let app = try mailApp()
                let id = try J.reqStr(req.params, "id")
                let account = J.optStr(req.params, "account")
                let msg = try findMessage(app, id: id, mailbox: J.optStr(req.params, "mailbox"), account: account)
                let targetName = try J.reqStr(req.params, "to_mailbox")
                guard J.optBool(req.params, "confirm") == true else {
                    return ["would_move": id, "to": targetName, "confirm_required": true]
                }
                let (target, resolved) = try findMailbox(app, account: account, name: targetName)
                try sbSend(app, selectors: ["move:to:", "transfer:to:"], args: [msg, target])
                return ["moved": true, "id": id, "mailbox": resolved]
            }
        }

        d.register("mail.mark") { req in
            try withWatchdog(30) {
                let app = try mailApp()
                let id = try J.reqStr(req.params, "id")
                let msg = try findMessage(app, id: id, mailbox: J.optStr(req.params, "mailbox"), account: J.optStr(req.params, "account"))
                if let read = J.optBool(req.params, "read") { msg.setValue(read, forKey: "readStatus") }
                if let flagged = J.optBool(req.params, "flagged") { msg.setValue(flagged, forKey: "flaggedStatus") }
                if let junk = J.optBool(req.params, "junk") { msg.setValue(junk, forKey: "junkMailStatus") }
                return ["marked": true, "id": id]
            }
        }

        d.register("mail.delete") { req in
            try withWatchdog(30) {
                let app = try mailApp()
                let id = try J.reqStr(req.params, "id")
                let msg = try findMessage(app, id: id, mailbox: J.optStr(req.params, "mailbox"), account: J.optStr(req.params, "account"))
                guard J.optBool(req.params, "confirm") == true else {
                    return ["would_delete": summary(msg, mailboxName: J.optStr(req.params, "mailbox") ?? ""), "confirm_required": true]
                }
                try sbSend(app, selectors: ["delete:"], args: [msg])
                return ["deleted": true, "id": id]
            }
        }

        d.register("mail.attachment_save") { req in
            try withWatchdog(60) {
                let app = try mailApp()
                let id = try J.reqStr(req.params, "id")
                let msg = try findMessage(app, id: id, mailbox: J.optStr(req.params, "mailbox"), account: J.optStr(req.params, "account"))
                guard let atts = sbElements(msg, "mailAttachments"), atts.count > 0 else {
                    throw BridgeError.notFound("Message has no attachments", app: "Mail")
                }
                let idx = (J.optInt(req.params, "index") ?? 0) + 1
                guard idx >= 1, idx <= atts.count, let att = sbAt(atts, idx) else {
                    throw BridgeError.invalidParams("attachment index out of range (0..\(atts.count - 1))")
                }
                let name = sbStr(att, "name")
                let dir = J.optStr(req.params, "dir") ?? NSTemporaryDirectory()
                let path = (dir as NSString).appendingPathComponent(name)
                do {
                    try sbSend(app, selectors: ["save:in:"], args: [att, path])
                } catch {
                    try sbSend(att, selectors: ["saveIn:", "save:"], args: [path])
                }
                return ["saved": true, "path": path]
            }
        }
    }
}
