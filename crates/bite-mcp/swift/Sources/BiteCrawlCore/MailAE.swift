// Raw Apple Event toolkit for Mail — validated patterns from scripts/probe_ae.swift.
//
// Object model gotcha: Mail's mailboxes are ACCOUNT-SCOPED. An app-level
// mailbox specifier (from: null) silently resolves to an empty set with no
// error — always chain account → mailbox → messages.

import Foundation
import AppKit

// Descriptor-type constants (module-level; shared with MailBulk)
let typeTypeD = DescType(typeType)
let typeSInt32D = DescType(typeSInt32)
let typeEnumeratedD = DescType(typeEnumerated)
let formWhoseD = DescType(0x77686f73)                   // 'whos'
let formNameD = DescType(0x6e616d65)                    // 'name'
let formIndexD = DescType(0x696e6478)                   // 'indx'
let formPropD = DescType(0x70726f70)                    // 'prop'
let typePropD = DescType(0x70726f70)
let typeCurrentContainerD = DescType(0x63636e74)        // 'ccnt'
let typeAbsoluteOrdinalD = DescType(0x6162736f)         // 'abso'

// Keywords
let keyDirectObject = AEKeyword(0x2d2d2d2d)             // '----'
let keyAEWant = AEKeyword(0x77616e74)                   // 'want'
let keyAEForm = AEKeyword(0x666f726d)                   // 'form'
let keyAESeld = AEKeyword(0x73656c64)                   // 'seld'
let keyAEFrom = AEKeyword(0x66726f6d)                   // 'from'
let keyTimeoutAttr = AEKeyword(0x2174696d)              // '!tim' (ticks)
let keyRequestedType = AEKeyword(0x72747970)            // 'rtyp'
let keyAEData = AEKeyword(0x64617461)                   // 'data'
let keyAEInsertHere = AEKeyword(0x696e7368)             // 'insh' (Mail move "to")
let keyAEObject1 = AEKeyword(0x6f626a31)                // 'obj1'
let keyAEObject2 = AEKeyword(0x6f626a32)                // 'obj2'
let keyAECompOperator = AEKeyword(0x72656c6f)           // 'relo'
let keyAELogicalTerms = AEKeyword(0x7465726d)           // 'term'
let kErrNumberKeyword = AEKeyword(0x6572726e)           // 'errn'
let kErrStringKeyword = AEKeyword(0x65727273)           // 'errs'

/// Last Apple Event error for diagnostics.
public var lastError: String?

// Classes / event ids / operators
let cMessage = FourCharCode(0x6d737367)                 // 'mssg'
let cMailbox = FourCharCode(0x6d627870)                 // 'mbxp'
let cAccount = FourCharCode(0x6d616374)                 // 'mact'
let kAECompareClass = FourCharCode(0x636f6d70)          // 'comp'
let kAECompareEventID = FourCharCode(0x63636d70)        // 'ccmp'
let kAEEquals = FourCharCode(0x3d202020)                // '=   '
let kAELessThanEquals = FourCharCode(0x3c3d2020)        // '<=  '
let kAEAnd = FourCharCode(0x414e4420)                   // 'AND '
let kAELogicalAndEventID = FourCharCode(0x6c616e64)     // 'land'
let kAELogicalClass = FourCharCode(0x6c6f6769)          // 'logi'
let kAEAll = FourCharCode(0x616c6c20)                   // 'all '

// Properties
let pAllD = FourCharCode(0x70414c4c)                    // 'pALL'
let pNameD = FourCharCode(0x706e616d)                   // 'pnam'
let pSubjectD = FourCharCode(0x7375626a)                // 'subj'
let pSenderD = FourCharCode(0x736e6472)                 // 'sndr'
let pDateSentD = FourCharCode(0x64726376)               // 'drcv'
let pReadD = FourCharCode(0x69737264)                   // 'isrd'
let pFlaggedD = FourCharCode(0x6973666c)                // 'isfl'
let pJunkD = FourCharCode(0x69736a6b)                   // 'isjk'
let pIDD = FourCharCode(0x49442020)                     // 'ID  '
let pContentD = FourCharCode(0x63746e74)                // 'ctnt'

func codeDesc(_ code: FourCharCode, _ type: DescType) -> NSAppleEventDescriptor {
    var c = code.bigEndian
    return NSAppleEventDescriptor(descriptorType: type, bytes: &c, length: 4)!
}

func intDesc(_ i: Int32) -> NSAppleEventDescriptor {
    var v = i.bigEndian
    return NSAppleEventDescriptor(descriptorType: typeSInt32D, bytes: &v, length: 4)!
}

func boolDesc(_ b: Bool) -> NSAppleEventDescriptor {
    NSAppleEventDescriptor(boolean: b)
}

func objSpec(want: FourCharCode, form: DescType, seld: NSAppleEventDescriptor?, from: NSAppleEventDescriptor?) -> NSAppleEventDescriptor {
    let spec = NSAppleEventDescriptor.record()
    spec.setDescriptor(codeDesc(want, typeTypeD), forKeyword: keyAEWant)
    var f = form.bigEndian
    spec.setDescriptor(NSAppleEventDescriptor(descriptorType: typeEnumeratedD, bytes: &f, length: 4)!, forKeyword: keyAEForm)
    if let seld { spec.setDescriptor(seld, forKeyword: keyAESeld) }
    spec.setDescriptor(from ?? NSAppleEventDescriptor.null(), forKeyword: keyAEFrom)
    return spec
}

func propSpec(_ property: FourCharCode, of target: NSAppleEventDescriptor?) -> NSAppleEventDescriptor {
    objSpec(want: typePropD, form: formPropD, seld: codeDesc(property, typeTypeD), from: target)
}

public enum MailAE {
    public static let pAll = pAllD
    public static let pName = pNameD
    public static let pSubject = pSubjectD
    public static let pSender = pSenderD
    public static let pDateSent = pDateSentD
    public static let pRead = pReadD
    public static let pFlagged = pFlaggedD
    public static let pJunk = pJunkD
    public static let pID = pIDD
    public static let pContent = pContentD
    public static let cMessageM = cMessage
    public static let cMailboxM = cMailbox
    public static let cAccountM = cAccount
    public static let formIndexM = formIndexD

    // ── target / health ──

    public static func mailTarget() -> NSAppleEventDescriptor? {
        guard let app = NSRunningApplication.runningApplications(withBundleIdentifier: "com.apple.mail").first else { return nil }
        return NSAppleEventDescriptor(processIdentifier: app.processIdentifier)
    }

    /// True when Mail answers a trivial Apple Event within `seconds`.
    public static func healthy(target: NSAppleEventDescriptor, seconds: Int32 = 5) -> Bool {
        let r = get(propSpec(pNameD, of: nil), target: target, timeoutSeconds: seconds)
        return r.reply?.stringValue != nil && r.seconds < Double(seconds)
    }

    // ── event send ──

    public static func get(_ direct: NSAppleEventDescriptor, target: NSAppleEventDescriptor, timeoutSeconds: Int32) -> (reply: NSAppleEventDescriptor?, seconds: Double) {
        let event = NSAppleEventDescriptor(
            eventClass: 0x636f7265, eventID: 0x67657464,
            targetDescriptor: target, returnID: AEReturnID(-1), transactionID: 0
        )
        event.setParam(direct, forKeyword: keyDirectObject)
        var ticks = timeoutSeconds * 60
        event.setAttribute(
            NSAppleEventDescriptor(descriptorType: typeSInt32D, bytes: &ticks, length: 4)!,
            forKeyword: keyTimeoutAttr
        )
        let t0 = Date()
        let reply = try? event.sendEvent(timeout: TimeInterval(timeoutSeconds))
        return (reply, Date().timeIntervalSince(t0))
    }

    public static func errInfo(_ reply: NSAppleEventDescriptor?) -> String? {
        guard let reply else { return "nil reply" }
        if let n = reply.forKeyword(kErrNumberKeyword)?.int32Value {
            let msg = reply.forKeyword(kErrStringKeyword)?.stringValue ?? ""
            return "AppleEvent error \(n): \(msg)"
        }
        return nil
    }

    /// One Apple Event returning a list of several properties of one object.
    public static func readProperties(_ properties: [FourCharCode], of spec: NSAppleEventDescriptor, target: NSAppleEventDescriptor, timeoutSeconds: Int32 = 20) -> [NSAppleEventDescriptor?]? {
        let list = NSAppleEventDescriptor.list()
        for p in properties {
            list.insert(propSpec(p, of: spec), at: Int(list.numberOfItems) + 1)
        }
        let (reply, _) = get(list, target: target, timeoutSeconds: timeoutSeconds)
        guard let reply, errInfo(reply) == nil, reply.descriptorType == DescType(0x6c697374) else { return nil }
        var out: [NSAppleEventDescriptor?] = []
        for i in 1...max(reply.numberOfItems, 1) where i <= reply.numberOfItems {
            out.append(reply.atIndex(i))
        }
        return out
    }

    /// 'cnte' event — count of an every-element / whose specifier.
    public static func count(_ everyOrWhoseSpec: NSAppleEventDescriptor, target: NSAppleEventDescriptor, timeoutSeconds: Int32 = 30) -> Int? {
        let event = NSAppleEventDescriptor(
            eventClass: 0x636f7265, eventID: 0x636e7465,
            targetDescriptor: target, returnID: AEReturnID(-1), transactionID: 0
        )
        event.setParam(everyOrWhoseSpec, forKeyword: keyDirectObject)
        var rtyp = typeSInt32D.bigEndian
        event.setParam(NSAppleEventDescriptor(descriptorType: typeTypeD, bytes: &rtyp, length: 4)!, forKeyword: keyRequestedType)
        guard let reply = try? event.sendEvent(timeout: TimeInterval(timeoutSeconds)) else {
            lastError = "sendEvent threw"
            return nil
        }
        if let errn = reply.forKeyword(kErrNumberKeyword)?.int32Value {
            let errs = reply.forKeyword(kErrStringKeyword)?.stringValue ?? ""
            lastError = "AE error \(errn): \(errs)"
            return nil
        }
        lastError = nil
        let v = reply.int32Value
        return v < 0 ? nil : Int(v)
    }

    /// "every <class> of <container>" specifier.
    public static func everySpec(want: FourCharCode, from: NSAppleEventDescriptor?) -> NSAppleEventDescriptor {
        let seld = NSAppleEventDescriptor(descriptorType: typeAbsoluteOrdinalD, bytes: unsafeBytes(of: kAEAll), length: 4)!
        return objSpec(want: want, form: formIndexD, seld: seld, from: from)
    }

    public static func unsafeBytes(of value: FourCharCode) -> UnsafeRawPointer {
        var v = value.bigEndian
        return withUnsafeBytes(of: &v) { UnsafeRawPointer($0.baseAddress!) }
    }

    // ── chains & enumeration ──

    public static func accountSpec(index: Int) -> NSAppleEventDescriptor {
        objSpec(want: cAccountM, form: formIndexM, seld: intDesc(Int32(index)), from: nil)
    }

    public static func mailboxSpec(name: String, account: NSAppleEventDescriptor?) -> NSAppleEventDescriptor {
        objSpec(want: cMailboxM, form: formNameD, seld: NSAppleEventDescriptor(string: name), from: account)
    }

    public static func messageSpec(index: Int, mailbox: NSAppleEventDescriptor) -> NSAppleEventDescriptor {
        objSpec(want: cMessageM, form: formIndexM, seld: intDesc(Int32(index)), from: mailbox)
    }

    public static func countAccounts(target: NSAppleEventDescriptor) -> Int? {
        count(everySpec(want: cAccountM, from: nil), target: target)
    }

    public static func countMailboxes(account: NSAppleEventDescriptor, target: NSAppleEventDescriptor) -> Int? {
        count(everySpec(want: cMailboxM, from: account), target: target)
    }

    public static func countMessages(mailbox: NSAppleEventDescriptor, target: NSAppleEventDescriptor) -> Int? {
        count(everySpec(want: cMessageM, from: mailbox), target: target)
    }

    public static func accountName(_ account: NSAppleEventDescriptor, target: NSAppleEventDescriptor) -> String? {
        let (reply, _) = get(propSpec(pNameD, of: account), target: target, timeoutSeconds: 20)
        guard let reply, errInfo(reply) == nil else { return nil }
        return reply.stringValue
    }

    public static func mailboxName(_ mailbox: NSAppleEventDescriptor, target: NSAppleEventDescriptor) -> String? {
        let (reply, _) = get(propSpec(pNameD, of: mailbox), target: target, timeoutSeconds: 20)
        guard let reply, errInfo(reply) == nil else { return nil }
        return reply.stringValue
    }

    public static func accountList(target: NSAppleEventDescriptor) -> [(name: String, spec: NSAppleEventDescriptor)]? {
        guard let n = countAccounts(target: target), n > 0 else { return nil }
        var out: [(String, NSAppleEventDescriptor)] = []
        for i in 1...n {
            let spec = accountSpec(index: i)
            guard let name = accountName(spec, target: target) else { continue }
            out.append((name, spec))
        }
        return out
    }

    public static func mailboxList(accountName: String, account: NSAppleEventDescriptor, target: NSAppleEventDescriptor) -> [(name: String, spec: NSAppleEventDescriptor)]? {
        guard let n = countMailboxes(account: account, target: target), n > 0 else { return nil }
        var out: [(String, NSAppleEventDescriptor)] = []
        for i in 1...n {
            let spec = objSpec(want: cMailbox, form: formIndexD, seld: intDesc(Int32(i)), from: account)
            guard let name = mailboxName(spec, target: target) else { continue }
            out.append((name, spec))
        }
        return out
    }

    // ── bulk primitives ──

    /// Send any pre-built event with a raised per-AE timeout.
    public static func sendBulk(_ event: NSAppleEventDescriptor, target: NSAppleEventDescriptor, timeoutSeconds: Int32) -> (ok: Bool, seconds: Double, error: String?) {
        var ticks = timeoutSeconds * 60
        event.setAttribute(
            NSAppleEventDescriptor(descriptorType: typeSInt32D, bytes: &ticks, length: 4)!,
            forKeyword: keyTimeoutAttr
        )
        let t0 = Date()
        guard let reply = try? event.sendEvent(timeout: TimeInterval(timeoutSeconds)) else {
            return (false, Date().timeIntervalSince(t0), "sendEvent failed")
        }
        if let errn = reply.forKeyword(kErrNumberKeyword)?.int32Value {
            let errs = reply.forKeyword(kErrStringKeyword)?.stringValue ?? ""
            return (false, Date().timeIntervalSince(t0), "AppleEvent error \(errn): \(errs)")
        }
        return (true, Date().timeIntervalSince(t0), nil)
    }

    public static func countWhose(_ selector: NSAppleEventDescriptor, target: NSAppleEventDescriptor, timeoutSeconds: Int32) -> Int? {
        count(selector, target: target, timeoutSeconds: timeoutSeconds)
    }

    /// set <property> of <specifier> to <value> — one Apple Event.
    public static func setStatus(_ property: FourCharCode, on selector: NSAppleEventDescriptor, to value: Bool,
                          target: NSAppleEventDescriptor, timeoutSeconds: Int32) -> (ok: Bool, error: String?) {
        let event = NSAppleEventDescriptor(
            eventClass: 0x636f7265, eventID: 0x73657464,
            targetDescriptor: target, returnID: AEReturnID(-1), transactionID: 0
        )
        event.setParam(propSpec(property, of: selector), forKeyword: keyDirectObject)
        event.setParam(boolDesc(value), forKeyword: keyAEData)
        let (ok, _, err) = sendBulk(event, target: target, timeoutSeconds: timeoutSeconds)
        return (ok, err)
    }

    /// "messages of <mailbox> whose <filter>" specifier. Empty filter →
    /// every-message specifier.
    public static func messageSelector(mailbox: NSAppleEventDescriptor, selection: BulkSelection, now: Date) -> NSAppleEventDescriptor {
        guard !selection.isEmpty else {
            return everySpec(want: cMessage, from: mailbox)
        }

        var tests: [[AEKeyword: NSAppleEventDescriptor]] = []
        func compEvent(_ test: [AEKeyword: NSAppleEventDescriptor]) -> NSAppleEventDescriptor {
            let ev = NSAppleEventDescriptor(
                eventClass: kAECompareClass, eventID: kAECompareEventID,
                targetDescriptor: NSAppleEventDescriptor.null(),
                returnID: AEReturnID(-1), transactionID: 0
            )
            if let o1 = test[keyAEObject1] { ev.setParam(o1, forKeyword: keyAEObject1) }
            if let relo = test[keyAECompOperator] { ev.setParam(relo, forKeyword: keyAECompOperator) }
            if let o2 = test[keyAEObject2] { ev.setParam(o2, forKeyword: keyAEObject2) }
            return ev
        }
        func eqTest(_ property: FourCharCode, _ value: NSAppleEventDescriptor) {
            let it = NSAppleEventDescriptor(descriptorType: typeCurrentContainerD, data: nil)!
            tests.append([
                keyAEObject1: objSpec(want: typePropD, form: formPropD, seld: codeDesc(property, typeTypeD), from: it),
                keyAECompOperator: codeDesc(kAEEquals, typeEnumeratedD),
                keyAEObject2: value,
            ])
        }
        func lteDateTest(_ property: FourCharCode, _ date: Date) {
            let it = NSAppleEventDescriptor(descriptorType: typeCurrentContainerD, data: nil)!
            tests.append([
                keyAEObject1: objSpec(want: typePropD, form: formPropD, seld: codeDesc(property, typeTypeD), from: it),
                keyAECompOperator: codeDesc(kAELessThanEquals, typeEnumeratedD),
                keyAEObject2: NSAppleEventDescriptor(date: date),
            ])
        }
        if let unread = selection.unread, unread {
            eqTest(pReadD, boolDesc(false))
        }
        if let days = selection.olderThanDays {
            lteDateTest(pDateSentD, now.addingTimeInterval(-Double(days) * 86400))
        }

        let test: NSAppleEventDescriptor
        if tests.count == 1 {
            test = compEvent(tests[0])
        } else {
            let terms = NSAppleEventDescriptor.list()
            for t in tests {
                terms.insert(compEvent(t), at: Int(terms.numberOfItems) + 1)
            }
            let logical = NSAppleEventDescriptor(
                eventClass: kAELogicalClass, eventID: kAELogicalAndEventID,
                targetDescriptor: NSAppleEventDescriptor.null(),
                returnID: AEReturnID(-1), transactionID: 0
            )
            logical.setParam(codeDesc(kAEAnd, typeEnumeratedD), forKeyword: keyDirectObject)
            logical.setParam(terms, forKeyword: keyAELogicalTerms)
            test = logical
        }

        let spec = NSAppleEventDescriptor.record()
        spec.setDescriptor(codeDesc(cMessage, typeTypeD), forKeyword: keyAEWant)
        var form = formWhoseD.bigEndian
        spec.setDescriptor(NSAppleEventDescriptor(descriptorType: typeEnumeratedD, bytes: &form, length: 4)!, forKeyword: keyAEForm)
        spec.setDescriptor(test, forKeyword: keyAESeld)
        spec.setDescriptor(mailbox, forKeyword: keyAEFrom)
        return spec
    }
}
