// Mirror crawlers for the native-framework apps (Calendar, Reminders,
// Contacts). These frameworks are already fast — the mirror exists so the
// unified `search` index is total across apps. Same JSONL batch pipeline
// as the Mail crawler.

import Foundation
import EventKit
import Contacts

public enum Mirrors {
    public static func write(_ records: [CrawlRecord], staging: URL, jobID: String, seq: inout Int) {
        guard !records.isEmpty else { return }
        MailCrawler.writeBatch(staging: staging, jobID: jobID, seq: seq, records: records)
        seq += 1
    }

    /// Calendar mirror: events from -90d to +180d across all calendars.
    public static func calendar(staging: URL, jobID: String, seq: inout Int, progress: @escaping (String, Int, Int) -> Void) {
        let store = EKEventStore()
        let sem = DispatchSemaphore(value: 0)
        if #available(macOS 14.0, *) {
            store.requestFullAccessToEvents { _, _ in sem.signal() }
        } else {
            store.requestAccess(to: .event) { _, _ in sem.signal() }
        }
        sem.wait()

        let now = Date()
        let from = now.addingTimeInterval(-90 * 86400)
        let to = now.addingTimeInterval(180 * 86400)
        let events = store.events(matching: store.predicateForEvents(withStart: from, end: to, calendars: nil))
        var records: [CrawlRecord] = []
        for e in events {
            let attendees = e.attendees?.compactMap { $0.name ?? $0.url.absoluteString }.prefix(20)
            records.append(CrawlRecord(
                app: "calendar",
                id: e.calendarItemIdentifier,
                account: e.calendar?.source?.title,
                container: e.calendar?.title,
                title: e.title,
                content: e.notes,
                participants: attendees?.isEmpty == false ? attendees?.joined(separator: ", ") : nil,
                start_ms: Int64(e.startDate.timeIntervalSince1970 * 1000),
                end_ms: Int64(e.endDate.timeIntervalSince1970 * 1000),
                updated_ms: nil,
                read: nil, flagged: nil, junk: nil, completed: nil,
                priority: nil,
                props: e.location.map { "{\"location\":\(jsonString($0))}" }
            ))
            if records.count >= 500 {
                write(records, staging: staging, jobID: jobID, seq: &seq)
                records.removeAll(keepingCapacity: true)
            }
        }
        write(records, staging: staging, jobID: jobID, seq: &seq)
        progress("running", CrawlState.shared.snapshot.processed, events.count)
    }

    /// Reminders mirror: all lists, completed + incomplete.
    public static func reminders(staging: URL, jobID: String, seq: inout Int, progress: @escaping (String, Int, Int) -> Void) {
        let store = EKEventStore()
        let sem = DispatchSemaphore(value: 0)
        if #available(macOS 14.0, *) {
            store.requestFullAccessToReminders { _, _ in sem.signal() }
        } else {
            store.requestAccess(to: .reminder) { _, _ in sem.signal() }
        }
        sem.wait()

        let lists = store.calendars(for: .reminder)
        var all: [EKReminder] = []
        let done = DispatchSemaphore(value: 0)
        store.fetchReminders(matching: store.predicateForReminders(in: lists)) { items in
            all = items ?? []
            done.signal()
        }
        done.wait()

        var records: [CrawlRecord] = []
        for r in all {
            records.append(CrawlRecord(
                app: "reminders",
                id: r.calendarItemIdentifier,
                account: r.calendar?.source?.title,
                container: r.calendar?.title,
                title: r.title,
                content: r.notes,
                participants: nil,
                start_ms: r.dueDateComponents?.date.map { Int64($0.timeIntervalSince1970 * 1000) },
                end_ms: nil,
                updated_ms: nil,
                read: nil, flagged: nil, junk: nil,
                completed: r.isCompleted,
                priority: r.priority > 0 ? r.priority : nil,
                props: nil
            ))
            if records.count >= 500 {
                write(records, staging: staging, jobID: jobID, seq: &seq)
                records.removeAll(keepingCapacity: true)
            }
        }
        write(records, staging: staging, jobID: jobID, seq: &seq)
        progress("running", CrawlState.shared.snapshot.processed, all.count)
    }

    /// Contacts mirror: all contacts in all containers.
    public static func contacts(staging: URL, jobID: String, seq: inout Int, progress: @escaping (String, Int, Int) -> Void) {
        let store = CNContactStore()
        let sem = DispatchSemaphore(value: 0)
        store.requestAccess(for: .contacts) { _, _ in sem.signal() }
        sem.wait()

        let keys = [
            CNContactIdentifierKey as CNKeyDescriptor,
            CNContactGivenNameKey as CNKeyDescriptor,
            CNContactFamilyNameKey as CNKeyDescriptor,
            CNContactOrganizationNameKey as CNKeyDescriptor,
            CNContactEmailAddressesKey as CNKeyDescriptor,
            CNContactPhoneNumbersKey as CNKeyDescriptor,
        ]
        var records: [CrawlRecord] = []
        let request = CNContactFetchRequest(keysToFetch: keys)
        request.keysToFetch = keys
        try? store.enumerateContacts(with: request) { c, _ in
            let emails = c.emailAddresses.map { $0.value as String }.prefix(10)
            let phones = c.phoneNumbers.map { $0.value.stringValue }.prefix(10)
            let name = [c.givenName, c.familyName].filter { !$0.isEmpty }.joined(separator: " ")
            records.append(CrawlRecord(
                app: "contacts",
                id: c.identifier,
                account: nil,
                container: nil,
                title: name.isEmpty ? c.organizationName : name,
                content: c.organizationName.isEmpty ? nil : c.organizationName,
                participants: emails.isEmpty && phones.isEmpty
                    ? nil
                    : (emails + phones).joined(separator: ", "),
                start_ms: nil, end_ms: nil, updated_ms: nil,
                read: nil, flagged: nil, junk: nil, completed: nil,
                priority: nil, props: nil
            ))
            if records.count >= 500 {
                write(records, staging: staging, jobID: jobID, seq: &seq)
                records.removeAll(keepingCapacity: true)
            }
        }
        write(records, staging: staging, jobID: jobID, seq: &seq)
        progress("running", CrawlState.shared.snapshot.processed, records.count)
    }

    static func jsonString(_ s: String) -> String {
        guard let data = try? JSONEncoder().encode(s), let out = String(data: data, encoding: .utf8) else { return "\"\"" }
        return out
    }
}
