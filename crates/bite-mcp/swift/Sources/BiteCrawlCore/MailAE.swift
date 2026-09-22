// Raw Apple Event toolkit for Mail — validated patterns from scripts/probe_ae.swift.
//
// Object model gotcha: Mail's mailboxes are ACCOUNT-SCOPED. An app-level
// mailbox specifier (from: null) silently resolves to an empty set with no
// error — always chain account → mailbox → messages.

import Foundation
import AppKit

enum MailAE {
    // descriptor type constants (AE framework exports these as DescType globals)
    private static let typeTypeD = DescType(typeType)
    private static let typeSInt32D = DescType(typeSInt32)
    private static let typeEnumeratedD = DescType(typeEnumerated)

    // keywords
    static let keyDirectObject = AEKeyword(0x2d2d2d2d)     // '----'
    static let keyAEWant = AEKeyword(0x77616e74)           // 'want'
    static let keyAEForm = AEKeyword(0x666f726d)           // 'form'
    static let keyAESeld = AEKeyword(0x73656c64)           // 'seld'
    static let keyAEFrom = AEKeyword(0x66726f6d)           // 'from'
    static let keyTimeoutAttr = AEKeyword(0x2174696d)      // '!tim' (ticks)
    static let keyRequestedType = AEKeyword(0x72747970)    // 'rtyp'
    // classes
    static let cMessage = FourCharCode(0x6d737367)         // 'mssg'
    static let cMailbox = FourCharCode(0x6d627870)         // 'mbxp'
    static let cAccount = FourCharCode(0x6d616374)         // 'mact'
    // properties
    static let pAll = FourCharCode(0x70414c4c)             // 'pALL'
    static let pName = FourCharCode(0x706e616d)            // 'pnam'
    static let pSubject = FourCharCode(0x7375626a)         // 'subj'
    static let pSender = FourCharCode(0x736e6472)          // 'sndr'
    static let pDateSent = FourCharCode(0x64726376)        // 'drcv'
    static let pRead = FourCharCode(0x69737264)            // 'isrd'
    static let pFlagged = FourCharCode(0x6973666c)         // 'isfl'
    static let pJunk = FourCharCode(0x69736a6b)            // 'isjk'
    static let pID = FourCharCode(0x49442020)              // 'ID  '
    static let pContent = FourCharCode(0x63746e74)         // 'ctnt'
    static let pUnreadCount = FourCharCode(0x6d627563)     // 'mbuc'
    // forms
    static let formName = DescType(0x6e616d65)             // 'name'
    static let formIndex = DescType(0x696e6478)            // 'indx'
    static let formProp = DescType(0x70726f70)             // 'prop'
    static let typePropD = DescType(0x70726f70)

    static func codeDesc(_ code: FourCharCode, _ type: DescType) -> NSAppleEventDescriptor {
        var c = code.bigEndian
        return NSAppleEventDescriptor(descriptorType: type, bytes: &c, length: 4)!
    }

    static func intDesc(_ i: Int32) -> NSAppleEventDescriptor {
        var v = i.bigEndian
        return NSAppleEventDescriptor(descriptorType: typeSInt32D, bytes: &v, length: 4)!
    }

    static func objSpec(want: FourCharCode, form: DescType, seld: NSAppleEventDescriptor?, from: NSAppleEventDescriptor?) -> NSAppleEventDescriptor {
        let spec = NSAppleEventDescriptor.record()
        spec.setDescriptor(codeDesc(want, typeTypeD), forKeyword: keyAEWant)
        var f = form.bigEndian
        spec.setDescriptor(NSAppleEventDescriptor(descriptorType: typeEnumeratedD, bytes: &f, length: 4)!, forKeyword: keyAEForm)
        if let seld { spec.setDescriptor(seld, forKeyword: keyAESeld) }
        spec.setDescriptor(from ?? NSAppleEventDescriptor.null(), forKeyword: keyAEFrom)
        return spec
    }

    static func propSpec(_ property: FourCharCode, of target: NSAppleEventDescriptor?) -> NSAppleEventDescriptor {
        objSpec(want: FourCharCode(typePropD), form: formProp, seld: codeDesc(property, typeTypeD), from: target)
    }

    // ── target ──
    static func mailTarget() -> NSAppleEventDescriptor? {
        guard let app = NSRunningApplication.runningApplications(withBundleIdentifier: "com.apple.mail").first else { return nil }
        return NSAppleEventDescriptor(processIdentifier: app.processIdentifier)
    }

    /// Send a 'getd' event. Returns reply + wall time. Errors surface in the
    /// reply's 'errn'/'errs' keywords (see errInfo).
    static func get(_ direct: NSAppleEventDescriptor, target: NSAppleEventDescriptor, timeoutSeconds: Int32) -> (reply: NSAppleEventDescriptor?, seconds: Double) {
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

    static func errInfo(_ reply: NSAppleEventDescriptor?) -> String? {
        guard let reply else { return "nil reply" }
        if let n = reply.forKeyword(AEKeyword(0x6572726e))?.int32Value {  // 'errn'
            let msg = reply.forKeyword(AEKeyword(0x65727273))?.stringValue ?? ""  // 'errs'
            return "AppleEvent error \(n): \(msg)"
        }
        return nil
    }

    // ── chains ──
    static func accountSpec(index: Int) -> NSAppleEventDescriptor {
        objSpec(want: cAccount, form: formIndex, seld: intDesc(Int32(index)), from: nil)
    }

    static func accountSpec(name: String) -> NSAppleEventDescriptor {
        objSpec(want: cAccount, form: formName, seld: NSAppleEventDescriptor(string: name), from: nil)
    }

    static func mailboxSpec(name: String, account: NSAppleEventDescriptor?) -> NSAppleEventDescriptor {
        objSpec(want: cMailbox, form: formName, seld: NSAppleEventDescriptor(string: name), from: account)
    }

    static func messageSpec(index: Int, mailbox: NSAppleEventDescriptor) -> NSAppleEventDescriptor {
        objSpec(want: cMessage, form: formIndex, seld: intDesc(Int32(index)), from: mailbox)
    }

    /// True when the target answers a trivial Apple Event within `seconds`.
    /// Mail's scripting layer oscillates between calm and saturated; the
    /// crawler gates all work behind this probe.
    static func healthy(target: NSAppleEventDescriptor, seconds: Int32 = 5) -> Bool {
        let r = get(propSpec(pName, of: nil), target: target, timeoutSeconds: seconds)
        return r.reply?.stringValue != nil && r.seconds < Double(seconds)
    }

    // ── typed reads ──

    /// One Apple Event returning a list of several properties of one object
    /// (a list of property specifiers as the direct object). Order matches
    /// the requested `properties` array.
    static func readProperties(_ properties: [FourCharCode], of spec: NSAppleEventDescriptor, target: NSAppleEventDescriptor, timeoutSeconds: Int32 = 20) -> [NSAppleEventDescriptor?]? {
        let list = NSAppleEventDescriptor.list()
        for p in properties {
            list.insert(propSpec(p, of: spec), at: Int(list.numberOfItems) + 1)
        }
        let (reply, _) = get(list, target: target, timeoutSeconds: timeoutSeconds)
        guard let reply, errInfo(reply) == nil, reply.descriptorType == DescType(0x6c697374) /* 'list' */ else { return nil }
        var out: [NSAppleEventDescriptor?] = []
        for i in 1...max(reply.numberOfItems, 1) where i <= reply.numberOfItems {
            out.append(reply.atIndex(i))
        }
        return out
    }

    /// 'cnte' event — count of an every-element specifier.
    static func count(_ everySpec: NSAppleEventDescriptor, target: NSAppleEventDescriptor, timeoutSeconds: Int32 = 30) -> Int? {
        let event = NSAppleEventDescriptor(
            eventClass: 0x636f7265, eventID: 0x636e7465,   // 'core'+'cnte'
            targetDescriptor: target, returnID: AEReturnID(-1), transactionID: 0
        )
        event.setParam(everySpec, forKeyword: keyDirectObject)
        var rtyp = DescType(typeSInt32).bigEndian
        event.setParam(NSAppleEventDescriptor(descriptorType: DescType(typeType), bytes: &rtyp, length: 4)!, forKeyword: keyRequestedType)
        guard let reply = try? event.sendEvent(timeout: TimeInterval(timeoutSeconds)) else { return nil }
        if reply.forKeyword(AEKeyword(0x6572726e)) != nil { return nil }  // 'errn'
        return reply.int32Value == -1 ? nil : Int(reply.int32Value)
    }

    /// "every <class> of <container>" specifier (formAbsolutePosition + kAEAll).
    static func everySpec(want: FourCharCode, from: NSAppleEventDescriptor?) -> NSAppleEventDescriptor {
        // seld = typeAbsoluteOrdinal descriptor containing kAEAll ('all ')
        var all = FourCharCode(0x616c6c20)
        let seld = NSAppleEventDescriptor(descriptorType: DescType(0x6162736f) /* typeAbsoluteOrdinal */, bytes: &all, length: 4)!
        return objSpec(want: want, form: formIndex, seld: seld, from: from)
    }

    static func countAccounts(target: NSAppleEventDescriptor) -> Int? {
        count(everySpec(want: cAccount, from: nil), target: target)
    }

    static func countMailboxes(account: NSAppleEventDescriptor, target: NSAppleEventDescriptor) -> Int? {
        count(everySpec(want: cMailbox, from: account), target: target)
    }

    static func countMessages(mailbox: NSAppleEventDescriptor, target: NSAppleEventDescriptor) -> Int? {
        count(everySpec(want: cMessage, from: mailbox), target: target)
    }

    static func accountName(_ account: NSAppleEventDescriptor, target: NSAppleEventDescriptor) -> String? {
        let (reply, _) = get(propSpec(pName, of: account), target: target, timeoutSeconds: 20)
        guard let reply, errInfo(reply) == nil else { return nil }
        return reply.stringValue
    }

    static func mailboxName(_ mailbox: NSAppleEventDescriptor, target: NSAppleEventDescriptor) -> String? {
        let (reply, _) = get(propSpec(pName, of: mailbox), target: target, timeoutSeconds: 20)
        guard let reply, errInfo(reply) == nil else { return nil }
        return reply.stringValue
    }

    /// Enumerate (account name, account spec) pairs. `count of accounts` is
    /// answered by Mail without deep enumeration — cheap.
    static func accountList(target: NSAppleEventDescriptor) -> [(name: String, spec: NSAppleEventDescriptor)]? {
        guard let n = countAccounts(target: target), n > 0 else { return nil }
        var out: [(String, NSAppleEventDescriptor)] = []
        for i in 1...n {
            let spec = accountSpec(index: i)
            guard let name = accountName(spec, target: target) else { continue }
            out.append((name, spec))
        }
        return out
    }

    /// Enumerate (mailbox name, mailbox spec) pairs for one account.
    static func mailboxList(accountName: String, account: NSAppleEventDescriptor, target: NSAppleEventDescriptor) -> [(name: String, spec: NSAppleEventDescriptor)]? {
        guard let n = countMailboxes(account: account, target: target), n > 0 else { return nil }
        var out: [(String, NSAppleEventDescriptor)] = []
        for i in 1...n {
            let spec = objSpec(want: cMailbox, form: formIndex, seld: intDesc(Int32(i)), from: account)
            guard let name = mailboxName(spec, target: target) else { continue }
            out.append((name, spec))
        }
        return out
    }
}
