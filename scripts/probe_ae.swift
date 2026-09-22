// Raw Apple Event probes against live Mail (read-only).
// Build: swiftc scripts/probe_ae.swift -o /tmp/probe_ae && /tmp/probe_ae
import Foundation
import AppKit

let keyDirectObject = AEKeyword(0x2d2d2d2d)     // '----'
let keyAEWant = AEKeyword(0x77616e74)           // 'want'
let keyAEForm = AEKeyword(0x666f726d)           // 'form'
let keyAESeld = AEKeyword(0x73656c64)           // 'seld'
let keyAEFrom = AEKeyword(0x66726f6d)           // 'from'
let formNameD = DescType(0x6e616d65)            // 'name'
let formIndexD = DescType(0x696e6478)           // 'indx'
let formPropD = DescType(0x70726f70)            // 'prop'
let typePropD = DescType(0x70726f70)            // 'prop'
let typeTypeD = DescType(typeType)
let typeSInt32D = DescType(typeSInt32)
let cMessage = FourCharCode(0x6d737367)         // 'mssg'
let cMailbox = FourCharCode(0x6d627870)         // 'mbxp'
let pAll = FourCharCode(0x70414c4c)             // 'pALL'
let pDateSent = FourCharCode(0x64726376)        // 'drcv' (Mail sdef "date sent")
let pSubject = FourCharCode(0x7375626a)         // 'subj'

func codeDesc(_ code: FourCharCode, _ type: DescType) -> NSAppleEventDescriptor {
    var c = code.bigEndian
    return NSAppleEventDescriptor(descriptorType: type, bytes: &c, length: 4)!
}

func intDesc(_ i: Int32) -> NSAppleEventDescriptor {
    var v = i.bigEndian
    return NSAppleEventDescriptor(descriptorType: typeSInt32D, bytes: &v, length: 4)!
}

func objSpec(want: FourCharCode, form: DescType, seld: NSAppleEventDescriptor?, from: NSAppleEventDescriptor?) -> NSAppleEventDescriptor {
    let spec = NSAppleEventDescriptor.record()
    spec.setDescriptor(codeDesc(want, typeTypeD), forKeyword: keyAEWant)
    var f = form.bigEndian
    spec.setDescriptor(NSAppleEventDescriptor(descriptorType: typeEnumerated, bytes: &f, length: 4)!, forKeyword: keyAEForm)
    if let seld { spec.setDescriptor(seld, forKeyword: keyAESeld) }
    spec.setDescriptor(from ?? NSAppleEventDescriptor.null(), forKeyword: keyAEFrom)
    return spec
}

func propSpec(_ property: FourCharCode, of target: NSAppleEventDescriptor?) -> NSAppleEventDescriptor {
    objSpec(want: typePropD.toFourChar(), form: formPropD, seld: codeDesc(property, typeTypeD), from: target)
}

extension DescType {
    func toFourChar() -> FourCharCode { FourCharCode(self) }
}

func fourCCName(_ kw: AEKeyword) -> String {
    let v = UInt32(bitPattern: Int32(kw))
    return String(format: "%c%c%c%c",
        UInt8((v >> 24) & 0xFF), UInt8((v >> 16) & 0xFF), UInt8((v >> 8) & 0xFF), UInt8(v & 0xFF))
}

// ── Mail target ──
guard let app = NSRunningApplication.runningApplications(withBundleIdentifier: "com.apple.mail").first else {
    print("Mail not running"); exit(1)
}
let target = NSAppleEventDescriptor(processIdentifier: app.processIdentifier)

func errInfo(_ reply: NSAppleEventDescriptor?) -> String {
    guard let reply else { return "nil reply" }
    let errn = reply.forKeyword(AEKeyword(0x6572726e))  // 'errn'
    let errs = reply.forKeyword(AEKeyword(0x65727273))  // 'errs'
    if let n = errn?.int32Value {
        return "error \(n): \(errs?.stringValue ?? "")"
    }
    return "no error"
}

func aeGet(_ direct: NSAppleEventDescriptor, timeoutSeconds: Int32) -> (reply: NSAppleEventDescriptor?, seconds: Double) {
    let event = NSAppleEventDescriptor(
        eventClass: 0x636f7265,                          // 'core'
        eventID: 0x67657464,                             // 'getd'
        targetDescriptor: target,
        returnID: AEReturnID(-1),
        transactionID: 0
    )
    event.setParam(direct, forKeyword: keyDirectObject)
    var ticks = timeoutSeconds * 60                     // '!tim' attribute is in ticks
    event.setAttribute(
        NSAppleEventDescriptor(descriptorType: typeSInt32D, bytes: &ticks, length: 4)!,
        forKeyword: AEKeyword(0x2174696d)               // '!tim'
    )
    let t0 = Date()
    do {
        let reply = try event.sendEvent(timeout: TimeInterval(timeoutSeconds))
        return (reply, Date().timeIntervalSince(t0))
    } catch {
        return (nil, Date().timeIntervalSince(t0))
    }
}

// ── specifiers ──
let inboxSpec = objSpec(want: cMailbox, form: formNameD, seld: NSAppleEventDescriptor(string: "INBOX"), from: nil)
let msg1 = objSpec(want: cMessage, form: formIndexD, seld: intDesc(1), from: inboxSpec)

print("probe 0: count every message of INBOX ('cnte')")
let allSpec = objSpec(want: cMessage, form: formIndexD, seld: codeDesc(FourCharCode(0x616c6c20), DescType(0x6162736f) /* typeAbsoluteOrdinal */), from: inboxSpec)
let cntEvent = NSAppleEventDescriptor(
    eventClass: 0x636f7265, eventID: 0x636e7465,
    targetDescriptor: target, returnID: AEReturnID(-1), transactionID: 0
)
cntEvent.setParam(allSpec, forKeyword: keyDirectObject)
var rtyp = typeSInt32D.bigEndian
cntEvent.setParam(NSAppleEventDescriptor(descriptorType: typeTypeD, bytes: &rtyp, length: 4)!, forKeyword: keyAERequestedType)
let r0 = aeGet(cntEvent, timeoutSeconds: 30)  // reuse aeGet wrapper (getd of direct? no —) 
_ = r0
// count is its own event; send directly:
let t0 = Date()
let cntReply = (try? cntEvent.sendEvent(timeout: TimeInterval(30))) ?? nil
print("  took \(String(format: "%.2f", Date().timeIntervalSince(t0)))s → count: \(cntReply?.int32Value ?? -1) | \(errInfo(cntReply))")

print("probe 1: get ALL properties ('pALL') of message 1 of INBOX")
let r1 = aeGet(propSpec(pAll, of: msg1), timeoutSeconds: 60)
print("  took \(String(format: "%.2f", r1.seconds))s | \(errInfo(r1.reply))")
if let reply = r1.reply {
    print("  type: \(fourCCName(reply.descriptorType))")
    if reply.descriptorType == DescType(0x7265636f) {  // 'reco'
        print("  record, \(reply.numberOfItems) items:")
        for probe in [pSubject, pDateSent, FourCharCode(0x69736764), FourCharCode(0x6973666c), FourCharCode(0x736e6472)] {
            let kw = AEKeyword(bitPattern: Int32(truncatingIfNeeded: probe))
            if let v = reply.forKeyword(kw) {
                print("    \(fourCCName(kw)) = \(v.stringValue ?? "<cplx>")")
            }
        }
    } else if let s = reply.stringValue {
        print("  string: \(s.prefix(60))")
    } else {
        print("  (other descriptor)")
    }
} else {
    print("  nil reply → timeout/failure")
}

print("probe 2: date sent of message 1 (single property)")
let r2 = aeGet(propSpec(pDateSent, of: msg1), timeoutSeconds: 30)
print("  took \(String(format: "%.2f", r2.seconds))s → \(r2.reply?.description.prefix(60).description ?? "nil") | \(errInfo(r2.reply))")

print("probe 3: subject of message 1")
let r3 = aeGet(propSpec(pSubject, of: msg1), timeoutSeconds: 30)
print("  took \(String(format: "%.2f", r3.seconds))s → \(r3.reply?.stringValue?.prefix(60) ?? "nil") | \(errInfo(r3.reply))")
