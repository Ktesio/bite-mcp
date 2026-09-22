import Foundation
import ScriptingBridge

/// Dynamic ScriptingBridge utilities.
///
/// We intentionally do NOT generate per-app sdef headers: we access sdef elements,
/// properties and commands by name at runtime (KVC + selectors). This keeps the
/// helper compilable without codegen and resilient to minor sdef changes across
/// macOS versions. All Apple interaction stays native (Apple Events via
/// ScriptingBridge) — no `osascript` anywhere.

func sbApp(bundleID: String, name: String) throws -> SBApplication {
    guard let app = SBApplication(bundleIdentifier: bundleID) else {
        throw BridgeError.appNotRunning(name)
    }
    // SBApplication.timeout is in TICKS (1/60 s), not seconds: 3600 = 60 s.
    // A small value silently zeroes out slow Apple Event queries (e.g.
    // `count of messages` on a large mailbox) instead of erroring — this was
    // the root cause of mail searches returning empty results.
    app.timeout = 3600
    return app
}

/// Element collection by sdef name, e.g. elements("accounts") on the application.
func sbElements(_ target: SBObject, _ name: String) -> SBElementArray? {
    target.value(forKey: name) as? SBElementArray
}

/// 1-based element access (AppleScript convention). The whole codebase indexes
/// through here so any indexing-convention surprise is fixed in one place.
func sbAt(_ arr: SBElementArray, _ i: Int) -> SBObject? {
    guard i >= 1, i <= arr.count else { return nil }
    return arr.object(at: i - 1) as? SBObject
}

/// Read an sdef property/element by name.
func sbGet(_ target: SBObject, _ key: String) -> Any? {
    target.value(forKey: key)
}

/// Read a string property; empty when missing.
func sbStr(_ target: SBObject, _ key: String) -> String {
    (sbGet(target, key) as? String) ?? ""
}

func sbDate(_ target: SBObject, _ key: String) -> Date? {
    sbGet(target, key) as? Date
}

func sbBool(_ target: SBObject, _ key: String) -> Bool? {
    sbGet(target, key) as? Bool
}

/// Create a scripting object of the app's sdef class with the given properties
/// (Apple's `classForScriptingClass` + `initWithProperties:` pattern).
func sbMake(_ app: SBApplication, className: String, properties: [String: Any]) throws -> SBObject {
    guard let cls = app.class(forScriptingClass: className) else {
        throw BridgeError.internalError("scripting class '\(className)' not found")
    }
    let instance = (cls as! NSObject.Type).init()
    let sel = NSSelectorFromString("initWithProperties:")
    guard instance.responds(to: sel),
          let obj = instance.perform(sel, with: properties)?.takeRetainedValue() as? SBObject else {
        throw BridgeError.internalError("cannot create scripting object '\(className)'")
    }
    return obj
}

/// Invoke a command trying several selector spellings (sdef → ObjC selector
/// conventions differ slightly between suites). Throws when none respond.
@discardableResult
func sbSend(_ target: SBObject, selectors: [String], args: [Any]) throws -> Any? {
    for name in selectors {
        let sel = NSSelectorFromString(name)
        guard target.responds(to: sel) else { continue }
        switch args.count {
        case 0:
            return target.perform(sel)?.takeUnretainedValue()
        case 1:
            return target.perform(sel, with: args[0])?.takeUnretainedValue()
        case 2:
            return target.perform(sel, with: args[0], with: args[1])?.takeUnretainedValue()
        default:
            continue
        }
    }
    throw BridgeError.internalError("no scripting selector answered among \(selectors.joined(separator: ", "))")
}

/// ScriptingBridge Apple Event calls time out with an ObjC exception when the
/// target app hangs; we cannot catch ObjC exceptions in Swift, so wrap the whole
/// handler in a watchdog that reports a `timeout` error instead of crashing.
func withWatchdog<T>(_ seconds: TimeInterval, _ body: @escaping () throws -> T) throws -> T {
    var result: Result<T, Error>?
    let sem = DispatchSemaphore(value: 0)
    DispatchQueue.global().async {
        result = Result { try body() }
        sem.signal()
    }
    if sem.wait(timeout: .now() + seconds) == .timedOut {
        throw BridgeError(code: "timeout", message: "Apple app call timed out after \(Int(seconds))s", fix: "Check that the target app is responsive, then retry.")
    }
    return try result!.get()
}
