import Foundation
import ScriptingBridge

/// Apple Notes via dynamic ScriptingBridge. Note bodies are HTML; the API
/// accepts Markdown and converts it (see Markdown.swift).

let NOTES_BUNDLE = "com.apple.Notes"

private func notesApp() throws -> SBApplication {
    try sbApp(bundleID: NOTES_BUNDLE, name: "Notes")
}

private func noteDict(_ n: SBObject) -> [String: Any] {
    var d: [String: Any] = [
        "id": String(describing: sbGet(n, "id") ?? ""),
        "title": sbStr(n, "name"),
        "updated": D.format(sbDate(n, "modificationDate")) ?? "",
    ]
    if let created = sbDate(n, "creationDate") { d["created"] = D.format(created) ?? "" }
    return d
}

private func allFolders(_ app: SBApplication) throws -> [(SBObject, String)] {
    var out: [(SBObject, String)] = []
    guard let accounts = sbElements(app, "accounts") else {
        throw BridgeError.appNotRunning("Notes")
    }
    for i in 1...accounts.count {
        guard let acct = sbAt(accounts, i) else { continue }
        let acctName = sbStr(acct, "name")
        guard let folders = sbElements(acct, "folders") else { continue }
        for j in 1...folders.count {
            guard let f = sbAt(folders, j) else { continue }
            out.append((f, acctName))
        }
    }
    return out
}

private func findFolder(_ app: SBApplication, name: String?, account: String?) throws -> (SBObject, String) {
    let folders = try allFolders(app)
    if let name {
        let hit = folders.first { sbStr($0.0, "name").caseInsensitiveCompare(name) == .orderedSame }
        guard let (f, acct) = hit else {
            throw BridgeError.notFound("No folder named '\(name)'", app: "Notes")
        }
        return (f, acct)
    }
    guard let first = folders.first else {
        throw BridgeError.notFound("No Notes folders found", app: "Notes")
    }
    return first
}

enum NotesBridge {
    static func register(_ d: Dispatcher) {
        d.register("notes.folders") { _ in
            try withWatchdog(30) {
                let app = try notesApp()
                var out: [[String: Any]] = []
                for (folder, acctName) in try allFolders(app) {
                    let count = sbElements(folder, "notes")?.count ?? 0
                    out.append(["name": sbStr(folder, "name"), "account": acctName, "count": count])
                }
                return ["folders": out]
            }
        }

        d.register("notes.search") { req in
            try withWatchdog(60) {
                let app = try notesApp()
                let text = J.optStr(req.params, "text")?.lowercased()
                let folderName = J.optStr(req.params, "folder")
                let limit = J.optInt(req.params, "limit") ?? 25
                var folders: [(SBObject, String)]
                if let folderName {
                    folders = [try findFolder(app, name: folderName, account: J.optStr(req.params, "account"))]
                } else {
                    folders = try allFolders(app)
                }
                let scanFolders = folders
                var out: [[String: Any]] = []
                var truncated = false
                scan: for (folder, _) in scanFolders {
                    guard let notes = sbElements(folder, "notes") else { continue }
                    var i = notes.count
                    var scanned = 0
                    while i >= 1, scanned < 300 {
                        guard let n = sbAt(notes, i) else { i -= 1; continue }
                        scanned += 1
                        i -= 1
                        let title = sbStr(n, "name")
                        let body = sbStr(n, "body")
                        if let text {
                            let hay = (title + " " + htmlToText(body)).lowercased()
                            guard hay.contains(text) else { continue }
                        }
                        var d = noteDict(n)
                        d["folder"] = sbStr(folder, "name")
                        if let snip = MailBridge.snippet(body) { d["snippet"] = snip }
                        out.append(d)
                        if out.count >= limit { truncated = true; break scan }
                    }
                }
                return ["notes": out, "truncated": truncated]
            }
        }

        d.register("notes.get") { req in
            try withWatchdog(30) {
                let app = try notesApp()
                let id = try J.reqStr(req.params, "id")
                guard let (note, folderName) = try findNote(app, id: id) else {
                    throw BridgeError.notFound("No note with id \(id)", app: "Notes")
                }
                var d = noteDict(note)
                d["folder"] = folderName
                d["body"] = sbStr(note, "body")
                d["markdown"] = htmlToMarkdown(sbStr(note, "body"))
                return ["note": d]
            }
        }

        d.register("notes.create") { req in
            try withWatchdog(30) {
                let app = try notesApp()
                let (folder, _) = try findFolder(app, name: J.optStr(req.params, "folder"), account: J.optStr(req.params, "account"))
                let title = J.optStr(req.params, "title") ?? "New Note"
                let md = J.optStr(req.params, "body") ?? J.optStr(req.params, "markdown") ?? ""
                let asHtml = J.optBool(req.params, "html") ?? false
                let body = asHtml ? md : markdownToHtml(md)
                let note = try sbMake(app, className: "note", properties: [
                    "name": title,
                    "body": body,
                ])
                if let notes = sbElements(folder, "notes") {
                    notes.add(note)
                }
                var d = noteDict(note)
                d["folder"] = sbStr(folder, "name")
                return ["note": d]
            }
        }

        d.register("notes.update") { req in
            try withWatchdog(30) {
                let app = try notesApp()
                let id = try J.reqStr(req.params, "id")
                guard let (note, _) = try findNote(app, id: id) else {
                    throw BridgeError.notFound("No note with id \(id)", app: "Notes")
                }
                if let title = J.optStr(req.params, "title") { note.setValue(title, forKey: "name") }
                let md = J.optStr(req.params, "body") ?? J.optStr(req.params, "markdown")
                if let md {
                    let asHtml = J.optBool(req.params, "html") ?? false
                    let html = asHtml ? md : markdownToHtml(md)
                    if J.optBool(req.params, "append") == true {
                        note.setValue(sbStr(note, "body") + html, forKey: "body")
                    } else {
                        note.setValue(html, forKey: "body")
                    }
                }
                if let folderName = J.optStr(req.params, "folder") {
                    let (target, _) = try findFolder(app, name: folderName, account: nil)
                    if let notes = sbElements(target, "notes") {
                        notes.add(note)
                    }
                }
                return ["note": noteDict(note)]
            }
        }

        d.register("notes.delete") { req in
            try withWatchdog(30) {
                let app = try notesApp()
                let id = try J.reqStr(req.params, "id")
                guard let (note, _) = try findNote(app, id: id) else {
                    throw BridgeError.notFound("No note with id \(id)", app: "Notes")
                }
                let preview = noteDict(note)
                guard J.optBool(req.params, "confirm") == true else {
                    return ["would_delete": preview, "confirm_required": true]
                }
                try sbSend(app, selectors: ["delete:"], args: [note])
                return ["deleted": true, "id": id]
            }
        }
    }

    static private func findNote(_ app: SBApplication, id: String) throws -> (SBObject, String)? {
        for (folder, _) in try allFolders(app) {
            guard let notes = sbElements(folder, "notes") else { continue }
            for i in 1...notes.count {
                guard let n = sbAt(notes, i) else { continue }
                if String(describing: sbGet(n, "id") ?? "") == id {
                    return (n, sbStr(folder, "name"))
                }
            }
        }
        return nil
    }
}
