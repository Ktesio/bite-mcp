import Foundation
import ScriptingBridge

/// Messages (iMessage/SMS) via dynamic ScriptingBridge.
///
/// Sending goes through buddies/chats exposed by the Messages sdef — a
/// recipient must be reachable in Messages (previous conversation or known
/// handle). Chat history read from `~/Library/Messages/chat.db` needs Full
/// Disk Access; when missing we degrade to ScriptingBridge-only data.

let MESSAGES_BUNDLE = "com.apple.iChat"

private func messagesApp() throws -> SBApplication {
    try sbApp(bundleID: MESSAGES_BUNDLE, name: "Messages")
}

private func normalizeHandle(_ s: String) -> String {
    s.lowercased().filter { $0.isLetter || $0.isNumber }
}

private func chatDict(_ chat: SBObject) -> [String: Any] {
    var d: [String: Any] = [
        "id": String(describing: sbGet(chat, "id") ?? ""),
    ]
    if let name = sbGet(chat, "name") as? String, !name.isEmpty { d["name"] = name }
    if let participants = sbElements(chat, "participants"), participants.count > 0 {
        var names: [String] = []
        for i in 1...participants.count {
            guard let p = sbAt(participants, i) else { continue }
            let handle = sbStr(p, "handle")
            let n = sbStr(p, "name")
            names.append(n.isEmpty ? handle : n)
        }
        d["participants"] = names
    }
    return d
}

enum MessagesBridge {
    static func register(_ d: Dispatcher) {
        d.register("messages.send") { req in
            try withWatchdog(60) {
                let app = try messagesApp()
                let to = try J.reqStr(req.params, "to")
                let text = try J.reqStr(req.params, "text")

                // find the buddy by handle across all services
                guard let services = sbElements(app, "services") else {
                    throw BridgeError.appNotRunning("Messages")
                }
                let want = normalizeHandle(to)
                for i in 1...services.count {
                    guard let service = sbAt(services, i), let buddies = sbElements(service, "buddies") else { continue }
                    for j in 1...buddies.count {
                        guard let b = sbAt(buddies, j) else { continue }
                        if normalizeHandle(sbStr(b, "handle")) == want {
                            try sbSend(app, selectors: ["send:to:"], args: [text, b])
                            return ["sent": true, "to": to]
                        }
                    }
                }
                throw BridgeError(
                    code: "not_found",
                    message: "No Messages buddy matching '\(to)'. Messages can only reach handles you have (or had) a conversation with — open Messages once and start a chat with this recipient, then retry.",
                    app: "Messages"
                )
            }
        }

        d.register("messages.chats_recent") { req in
            try withWatchdog(30) {
                let app = try messagesApp()
                guard let chats = sbElements(app, "chats") else {
                    throw BridgeError.appNotRunning("Messages")
                }
                let limit = J.optInt(req.params, "limit") ?? 20
                var out: [[String: Any]] = []
                var i = chats.count
                while i >= 1, out.count < limit {
                    guard let chat = sbAt(chats, i) else { i -= 1; continue }
                    i -= 1
                    var d = chatDict(chat)
                    if let messages = sbElements(chat, "messages"), messages.count > 0,
                       let last = sbAt(messages, messages.count) {
                        var lm: [String: Any] = ["text": sbStr(last, "body") ]
                        lm["from"] = sbStr(last, "sender")
                        if let t = sbDate(last, "time") { lm["time"] = D.format(t) ?? "" }
                        d["last_message"] = lm
                    }
                    out.append(d)
                }
                return ["chats": out]
            }
        }

        d.register("messages.history") { req in
            try withWatchdog(60) {
                let app = try messagesApp()
                let chatId = try J.reqStr(req.params, "chat_id")
                let limit = J.optInt(req.params, "limit") ?? 100
                guard let chats = sbElements(app, "chats") else {
                    throw BridgeError.appNotRunning("Messages")
                }
                var target: SBObject?
                for i in 1...chats.count {
                    guard let c = sbAt(chats, i) else { continue }
                    if String(describing: sbGet(c, "id") ?? "") == chatId { target = c; break }
                }
                guard let chat = target else {
                    // fall back to chat.db when the chat isn't active in Messages
                    if let rows = try? historyFromDb(chatId: chatId, limit: limit) {
                        return ["messages": rows, "source": "chat.db"]
                    }
                    throw BridgeError.notFound("No active chat with id \(chatId)", app: "Messages")
                }
                guard let messages = sbElements(chat, "messages") else {
                    throw BridgeError.notFound("Cannot read messages of chat \(chatId)", app: "Messages")
                }
                var out: [[String: Any]] = []
                var i = messages.count
                while i >= 1, out.count < limit {
                    guard let m = sbAt(messages, i) else { i -= 1; continue }
                    i -= 1
                    var d: [String: Any] = [
                        "id": String(describing: sbGet(m, "id") ?? ""),
                        "text": sbStr(m, "body"),
                        "from": sbStr(m, "sender"),
                    ]
                    if let t = sbDate(m, "time") { d["time"] = D.format(t) ?? "" }
                    out.append(d)
                }
                return ["messages": out.reversed(), "source": "messages_app"]
            }
        }
    }
}

/// Read chat history straight from the Messages sqlite store (needs Full Disk Access).
private func historyFromDb(chatId: String, limit: Int) throws -> [[String: Any]] {
    let dbPath = NSString(string: "~/Library/Messages/chat.db").expandingTildeInPath
    guard FileManager.default.isReadableFile(atPath: dbPath) else {
        throw BridgeError.fdaRequired("Reading full chat history needs Full Disk Access for your terminal (chat.db is not readable).")
    }
    // Introspect safely: only standard columns, no dynamic SQL parts from input
    // other than an integer cast of the chat id.
    guard let chatNum = Int(chatId.components(separatedBy: CharacterSet.decimalDigits.inverted).joined()) else {
        throw BridgeError.notFound("chat id '\(chatId)' is not a numeric Messages chat", app: "Messages")
    }
    let sqlite = "/usr/bin/sqlite3"
    guard FileManager.default.isExecutableFile(atPath: sqlite) else {
        throw BridgeError.internalError("sqlite3 not found at \(sqlite)")
    }
    let query = "SELECT m.ROWID, m.text, m.is_from_me, datetime(m.date/1000000000 + 978307200, 'unixepoch') FROM message m JOIN chat_message_join cmj ON cmj.message_id = m.ROWID WHERE cmj.chat_id = \(chatNum) ORDER BY m.date DESC LIMIT \(limit);"
    let proc = Process()
    proc.executableURL = URL(fileURLWithPath: sqlite)
    proc.arguments = ["-json", dbPath, query]
    let pipe = Pipe()
    proc.standardOutput = pipe
    proc.standardError = FileHandle.nullDevice
    try proc.run()
    let data = pipe.fileHandleForReading.readDataToEndOfFile()
    proc.waitUntilExit()
    guard proc.terminationStatus == 0,
          let arr = (try? JSONSerialization.jsonObject(with: data)) as? [[String: Any]] else {
        return []
    }
    return arr.map { row in
        [
            "id": String(describing: row["ROWID"] ?? ""),
            "text": row["text"] ?? "",
            "from_me": (row["is_from_me"] as? NSNumber)?.intValue == 1,
            "time": row["datetime(m.date/1000000000 + 978307200, 'unixepoch')"] ?? "",
        ]
    }
    .reversed()
}
