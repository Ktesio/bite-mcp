import Foundation

/// Wire protocol shared with the Rust control plane (see docs/protocol.md).
/// Line-delimited JSON over stdin/stdout.
///
///   helper -> host : {"method":"hello","params":{"protocol":1,"version":...,"capabilities":[...]}}
///   host -> helper : {"id":N,"method":"app.verb","params":{...}}
///   helper -> host : {"id":N,"result":{...}}  or  {"id":N,"error":{"code","message","app","fix"}}

let PROTOCOL_VERSION = 1
let HELPER_VERSION = "0.1.0"

let CAPABILITIES = ["calendar", "reminders", "contacts", "mail", "notes", "messages"]

struct BridgeError: Error, CustomStringConvertible {
    let code: String
    let message: String
    var app: String?
    var fix: String?

    var description: String { message }

    func toDict() -> [String: Any] {
        var d: [String: Any] = ["code": code, "message": message]
        if let app { d["app"] = app }
        if let fix { d["fix"] = fix }
        return d
    }

    static func invalidParams(_ m: String) -> BridgeError {
        BridgeError(code: "invalid_params", message: m)
    }

    static func internalError(_ m: String) -> BridgeError {
        BridgeError(code: "internal", message: m)
    }

    static func notFound(_ m: String, app: String) -> BridgeError {
        BridgeError(code: "not_found", message: m, app: app, fix: nil)
    }

    static func permissionDenied(app: String, pane: String) -> BridgeError {
        BridgeError(
            code: "permission_denied",
            message: "Access to \(app) was denied. Grant it in System Settings → Privacy & Security, then retry.",
            app: app,
            fix: "open 'x-apple.systempreferences:com.apple.preference.security?Privacy_\(pane)'"
        )
    }

    static func notDetermined(app: String, pane: String) -> BridgeError {
        BridgeError(
            code: "permission_denied",
            message: "\(app) access has not been granted yet. Approve the system prompt (or run `bite doctor`) and retry.",
            app: app,
            fix: "open 'x-apple.systempreferences:com.apple.preference.security?Privacy_\(pane)'"
        )
    }

    static func fdaRequired(_ m: String) -> BridgeError {
        BridgeError(
            code: "fda_required",
            message: m,
            fix: "System Settings → Privacy & Security → Full Disk Access → add your terminal app (or `bite`)"
        )
    }

    static func appNotRunning(_ name: String) -> BridgeError {
        BridgeError(
            code: "app_not_running",
            message: "Could not script \(name). Is it installed?",
            app: name
        )
    }
}

struct Incoming {
    let id: Int64?
    let method: String
    let params: [String: Any]
}

enum Envelope {
    /// Parse one NDJSON line into a request. Returns nil for notifications / unparseable lines.
    static func parse(_ data: Data) -> Incoming? {
        guard let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { return nil }
        guard let method = obj["method"] as? String, !method.isEmpty else { return nil }
        let id: Int64?
        switch obj["id"] {
        case let n as Int: id = Int64(n)
        case let n as Int64: id = n
        case let n as Double: id = Int64(n)
        case let s as String: id = Int64(s)
        default: id = nil
        }
        let params = (obj["params"] as? [String: Any]) ?? [:]
        return Incoming(id: id, method: method, params: params)
    }
}

typealias Handler = (Incoming) throws -> [String: Any]

final class Dispatcher {
    private var handlers: [String: Handler] = [:]
    private let lock = NSLock()

    func register(_ method: String, _ handler: @escaping Handler) {
        lock.lock(); defer { lock.unlock() }
        handlers[method] = handler
    }

    func handler(for method: String) -> Handler? {
        lock.lock(); defer { lock.unlock() }
        return handlers[method]
    }

    /// Build a full response envelope for one parsed request.
    func respond(_ req: Incoming) -> [String: Any] {
        guard let id = req.id else { return [:] }  // notifications are ignored
        guard let handler = handler(for: req.method) else {
            return ["id": id, "error": BridgeError(code: "unknown_method", message: "No handler for \(req.method)").toDict()]
        }
        do {
            let result = try handler(req)
            return ["id": id, "result": result]
        } catch let e as BridgeError {
            return ["id": id, "error": e.toDict()]
        } catch {
            return ["id": id, "error": BridgeError(code: "internal", message: "\(error)").toDict()]
        }
    }
}

/// Output writer, serialized; every response is one line + "\n".
final class LineWriter {
    private let handle: FileHandle
    private let lock = NSLock()

    init(_ handle: FileHandle) { self.handle = handle }

    func write(_ obj: [String: Any]) {
        guard JSONSerialization.isValidJSONObject(obj), let data = try? JSONSerialization.data(withJSONObject: obj) else { return }
        var line = data
        line.append(0x0A)
        lock.lock()
        do { try handle.write(contentsOf: line) } catch { /* stdout closed; exit quietly */ }
        lock.unlock()
    }
}

/// Typed accessors for request params.
enum J {
    static func reqStr(_ p: [String: Any], _ key: String) throws -> String {
        guard let v = optStr(p, key) else { throw BridgeError.invalidParams("missing required string field '\(key)'") }
        return v
    }
    static func optStr(_ p: [String: Any], _ key: String) -> String? {
        (p[key] as? String)?.isEmpty == false ? p[key] as? String : nil
    }
    static func optBool(_ p: [String: Any], _ key: String) -> Bool? { p[key] as? Bool }
    static func optInt(_ p: [String: Any], _ key: String) -> Int? { (p[key] as? NSNumber)?.intValue }
    static func optDouble(_ p: [String: Any], _ key: String) -> Double? { (p[key] as? NSNumber)?.doubleValue }
    static func optDict(_ p: [String: Any], _ key: String) -> [String: Any]? { p[key] as? [String: Any] }
    static func optStrArray(_ p: [String: Any], _ key: String) -> [String] { (p[key] as? [Any])?.compactMap { $0 as? String } ?? [] }
    static func optAnyArray(_ p: [String: Any], _ key: String) -> [Any] { (p[key] as? [Any]) ?? [] }
}
