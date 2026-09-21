import Foundation
import Contacts

/// Contacts.framework bridge (native API, not ScriptingBridge — the AB sdef is read-only and deprecated).
final class CN {
    static let shared = CN()
    let store = CNContactStore()

    func ensure() throws {
        let status = CNContactStore.authorizationStatus(for: .contacts)
        guard status != .denied, status != .restricted else {
            throw BridgeError.permissionDenied(app: "Contacts", pane: "Contacts")
        }
        guard status != .authorized else { return }
        var granted = false
        let sem = DispatchSemaphore(value: 0)
        store.requestAccess(for: .contacts) { ok, _ in
            granted = ok
            sem.signal()
        }
        sem.wait()
        guard granted else { throw BridgeError.permissionDenied(app: "Contacts", pane: "Contacts") }
    }
}

private let contactKeys: [CNKeyDescriptor] = [
    CNContactIdentifierKey as CNKeyDescriptor,
    CNContactGivenNameKey as CNKeyDescriptor,
    CNContactFamilyNameKey as CNKeyDescriptor,
    CNContactOrganizationNameKey as CNKeyDescriptor,
    CNContactPhoneNumbersKey as CNKeyDescriptor,
    CNContactEmailAddressesKey as CNKeyDescriptor,
    CNContactUrlAddressesKey as CNKeyDescriptor,
    CNContactNoteKey as CNKeyDescriptor,
    CNContactBirthdayKey as CNKeyDescriptor,
]

func labeledArray<T>(_ items: [T], label: (T) -> String?, value: (T) -> String) -> [[String: String]] {
    items.map { ["label": label($0) ?? "other", "value": value($0)] }
}

func contactDict(_ c: CNContact) -> [String: Any] {
    var d: [String: Any] = [
        "id": c.identifier,
        "first": c.givenName,
        "last": c.familyName,
    ]
    if !c.organizationName.isEmpty { d["org"] = c.organizationName }
    let emails = labeledArray(c.emailAddresses, label: { $0.label }, value: { $0.value as String })
    if !emails.isEmpty { d["emails"] = emails }
    let phones = labeledArray(c.phoneNumbers, label: { $0.label }, value: { $0.value.stringValue })
    if !phones.isEmpty { d["phones"] = phones }
    let urls = labeledArray(c.urlAddresses, label: { $0.label }, value: { $0.value as String })
    if !urls.isEmpty { d["urls"] = urls }
    if !c.note.isEmpty { d["note"] = c.note }
    if let b = c.birthday, let month = b.month, let day = b.day {
        if let year = b.year {
            d["birthday"] = String(format: "%04d-%02d-%02d", year, month, day)
        } else {
            d["birthday"] = String(format: "--%02d-%02d", month, day)
        }
    }
    return d
}

private func applyContactFields(_ c: CNMutableContact, _ p: [String: Any]) throws {
    if let v = J.optStr(p, "first") { c.givenName = v }
    if let v = J.optStr(p, "last") { c.familyName = v }
    if let v = J.optStr(p, "org") { c.organizationName = v }
    if let v = J.optStr(p, "note") { c.note = v }
    if let pairs = p["emails"] as? [[String: String]] {
        c.emailAddresses = pairs.compactMap { pair in
            guard let value = pair["value"], !value.isEmpty else { return nil }
            return CNLabeledValue(label: pair["label"] ?? "other", value: value as NSString)
        }
    }
    if let pairs = p["phones"] as? [[String: String]] {
        c.phoneNumbers = pairs.compactMap { pair in
            guard let value = pair["value"], !value.isEmpty else { return nil }
            return CNLabeledValue(label: pair["label"] ?? "other", value: CNPhoneNumber(stringValue: value))
        }
    }
    if let pairs = p["urls"] as? [[String: String]] {
        c.urlAddresses = pairs.compactMap { pair in
            guard let value = pair["value"], !value.isEmpty else { return nil }
            return CNLabeledValue(label: pair["label"] ?? "other", value: value as NSString)
        }
    }
}

func registerContactsHandlers(_ d: Dispatcher) {
    d.register("contacts.search") { req in
        try CN.shared.ensure()
        let query = try J.reqStr(req.params, "query").lowercased()
        let limit = J.optInt(req.params, "limit") ?? 25
        let pred = CNContact.predicateForContactsInContainer(withIdentifier: CN.shared.store.defaultContainerIdentifier())
        let found = try CN.shared.store.unifiedContacts(matching: pred, keysToFetch: contactKeys)
        var out: [[String: Any]] = []
        var truncated = false
        for c in found {
            let hay = "\(c.givenName) \(c.familyName) \(c.organizationName) "
                + c.emailAddresses.map { $0.value as String }.joined(separator: " ") + " "
                + c.phoneNumbers.map { $0.value.stringValue }.joined(separator: " ")
            if hay.lowercased().contains(query) {
                if out.count >= limit { truncated = true; break }
                out.append(contactDict(c))
            }
        }
        return ["contacts": out, "truncated": truncated]
    }

    d.register("contacts.get") { req in
        try CN.shared.ensure()
        let id = try J.reqStr(req.params, "id")
        let pred = CNContact.predicateForContacts(withIdentifiers: [id])
        guard let c = try CN.shared.store.unifiedContacts(matching: pred, keysToFetch: contactKeys).first else {
            throw BridgeError.notFound("No contact with id \(id)", app: "Contacts")
        }
        return ["contact": contactDict(c)]
    }

    d.register("contacts.create") { req in
        try CN.shared.ensure()
        let c = CNMutableContact()
        try applyContactFields(c, req.params)
        let save = CNSaveRequest()
        save.add(c, toContainerWithIdentifier: CN.shared.store.defaultContainerIdentifier())
        try CN.shared.store.execute(save)
        return ["contact": contactDict(c)]
    }

    d.register("contacts.update") { req in
        try CN.shared.ensure()
        let id = try J.reqStr(req.params, "id")
        let pred = CNContact.predicateForContacts(withIdentifiers: [id])
        guard let fetched = try CN.shared.store.unifiedContacts(matching: pred, keysToFetch: contactKeys).first else {
            throw BridgeError.notFound("No contact with id \(id)", app: "Contacts")
        }
        let c = fetched.mutableCopy() as! CNMutableContact
        try applyContactFields(c, req.params)
        let save = CNSaveRequest()
        save.update(c)
        try CN.shared.store.execute(save)
        return ["contact": contactDict(c)]
    }

    d.register("contacts.delete") { req in
        try CN.shared.ensure()
        let id = try J.reqStr(req.params, "id")
        let pred = CNContact.predicateForContacts(withIdentifiers: [id])
        guard let fetched = try CN.shared.store.unifiedContacts(matching: pred, keysToFetch: contactKeys).first else {
            throw BridgeError.notFound("No contact with id \(id)", app: "Contacts")
        }
        guard J.optBool(req.params, "confirm") == true else {
            return ["would_delete": contactDict(fetched), "confirm_required": true]
        }
        let save = CNSaveRequest()
        save.delete(fetched.mutableCopy() as! CNMutableContact)
        try CN.shared.store.execute(save)
        return ["deleted": true, "id": id]
    }

    d.register("contacts.groups") { _ in
        try CN.shared.ensure()
        let groups = try CN.shared.store.groups(matching: nil)
        var out: [[String: Any]] = []
        for g in groups {
            let pred = CNContact.predicateForContactsInGroup(withIdentifier: g.identifier)
            let count = (try? CN.shared.store.unifiedContacts(matching: pred, keysToFetch: [CNContactIdentifierKey as CNKeyDescriptor]).count) ?? 0
            out.append(["id": g.identifier, "name": g.name, "members": count])
        }
        return ["groups": out]
    }
}
