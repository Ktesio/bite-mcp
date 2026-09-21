import ScriptingBridge
import Foundation

guard let finder = SBApplication(bundleIdentifier: "com.apple.finder") else {
    print("PROBE-RESULT: no_finder"); exit(0)
}
finder.timeout = 15
// app-level property read: proves the Apple Event channel works at all
let fname = finder.value(forKey: "name") as? String
print("finder.name: \(fname ?? "nil")")
guard let home = finder.value(forKey: "home") as? SBObject else { print("PROBE-RESULT: no_home"); exit(0) }
guard let folders = home.value(forKey: "entireContents") as? SBElementArray else {
    print("PROBE-RESULT: no_folders"); exit(0)
}
print("count: \(folders.count)")
var enumerated: [String] = []
for d in folders.prefix(3) { enumerated.append((d as? SBObject).flatMap { $0.value(forKey: "name") as? String } ?? "?") }
print("enumerated: \(enumerated)")
let at0 = (folders.object(at: 0) as? SBObject).flatMap { $0.value(forKey: "name") as? String }
let at1 = (folders.object(at: 1) as? SBObject).flatMap { $0.value(forKey: "name") as? String }
print("object(at:0): \(at0 ?? "nil")")
print("object(at:1): \(at1 ?? "nil")")
if enumerated.count >= 2 {
    if at0 == enumerated[0] && at1 == enumerated[1] { print("PROBE-RESULT: zero_based") }
    else if at0 == enumerated[1] { print("PROBE-RESULT: one_based") }
    else { print("PROBE-RESULT: inconclusive") }
}
