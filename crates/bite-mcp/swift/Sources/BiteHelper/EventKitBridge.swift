import Foundation
import EventKit

/// Persistent EventKit store with TCC-aware authorization.
final class EKStore {
    static let shared = EKStore()
    let store = EKEventStore()
    private var eventsAuthorized = false
    private var remindersAuthorized = false

    func ensureEvents() throws {
        guard !eventsAuthorized else { return }
        let status = EKEventStore.authorizationStatus(for: .event)
        guard status != .denied, status != .restricted else {
            throw BridgeError.permissionDenied(app: "Calendar", pane: "Calendars")
        }
        var granted = false
        let sem = DispatchSemaphore(value: 0)
        if #available(macOS 14.0, *) {
            store.requestFullAccessToEvents { ok, _ in granted = ok; sem.signal() }
        } else {
            store.requestAccess(to: .event) { ok, _ in granted = ok; sem.signal() }
        }
        sem.wait()
        guard granted else { throw BridgeError.permissionDenied(app: "Calendar", pane: "Calendars") }
        eventsAuthorized = true
    }

    func ensureReminders() throws {
        guard !remindersAuthorized else { return }
        let status = EKEventStore.authorizationStatus(for: .reminder)
        guard status != .denied, status != .restricted else {
            throw BridgeError.permissionDenied(app: "Reminders", pane: "Reminders")
        }
        var granted = false
        let sem = DispatchSemaphore(value: 0)
        if #available(macOS 14.0, *) {
            store.requestFullAccessToReminders { ok, _ in granted = ok; sem.signal() }
        } else {
            store.requestAccess(to: .reminder) { ok, _ in granted = ok; sem.signal() }
        }
        sem.wait()
        guard granted else { throw BridgeError.permissionDenied(app: "Reminders", pane: "Reminders") }
        remindersAuthorized = true
    }
}

// MARK: - Calendar

func eventDict(_ e: EKEvent) -> [String: Any] {
    var d: [String: Any] = [
        "id": e.eventIdentifier,
        "calendar_id": e.calendar.calendarIdentifier,
        "calendar": e.calendar.title,
        "title": e.title ?? "",
        "start": D.format(e.startDate) ?? "",
        "end": D.format(e.endDate) ?? "",
        "all_day": e.isAllDay,
    ]
    if let v = e.location, !v.isEmpty { d["location"] = v }
    if let v = e.notes, !v.isEmpty { d["notes"] = v }
    if let v = e.url { d["url"] = v.absoluteString }
    if let v = D.format(e.creationDate) { d["created"] = v }
    if let v = D.format(e.lastModifiedDate) { d["modified"] = v }
    if e.status == .canceled { d["status"] = "canceled" }
    else if e.status == .tentative { d["status"] = "tentative" }
    if let organizer = e.organizer?.name, !organizer.isEmpty {
        d["organizer"] = organizer
    }
    if let rule = e.recurrenceRules?.first {
        d["recurrence"] = recurrenceDict(rule)
    }
    return d
}

func recurrenceDict(_ r: EKRecurrenceRule) -> [String: Any] {
    let freq: String
    switch r.frequency {
    case .daily: freq = "daily"
    case .weekly: freq = "weekly"
    case .monthly: freq = "monthly"
    case .yearly: freq = "yearly"
    @unknown default: freq = "daily"
    }
    var d: [String: Any] = ["freq": freq, "interval": r.interval]
    if let end = r.recurrenceEnd?.endDate, let until = D.format(end), !until.isEmpty {
        d["until"] = until
    }
    return d
}

func makeRecurrenceRule(_ d: [String: Any]) throws -> EKRecurrenceRule {
    let freqStr = (d["freq"] as? String)?.lowercased() ?? "weekly"
    let freq: EKRecurrenceFrequency
    switch freqStr {
    case "daily": freq = .daily
    case "weekly": freq = .weekly
    case "monthly": freq = .monthly
    case "yearly": freq = .yearly
    default: throw BridgeError.invalidParams("recurrence.freq must be daily|weekly|monthly|yearly")
    }
    let interval = (d["interval"] as? NSNumber)?.intValue ?? 1
    guard interval >= 1 else { throw BridgeError.invalidParams("recurrence.interval must be >= 1") }
    var end: EKRecurrenceEnd?
    if let until = try D.optDate(d, "until") {
        end = EKRecurrenceEnd(end: until)
    } else if let count = (d["count"] as? NSNumber)?.intValue, count >= 1 {
        end = EKRecurrenceEnd(occurrenceCount: count)
    }
    return EKRecurrenceRule(recurrenceWith: freq, interval: interval, end: end)
}

func pickCalendar(store: EKEventStore, id: String?, title: String? = nil) -> EKCalendar? {
    let cals = store.calendars(for: .event)
    if let id {
        return cals.first { $0.calendarIdentifier == id }
    }
    if let title {
        return cals.first { $0.title.caseInsensitiveCompare(title) == .orderedSame }
    }
    return cals.first { $0.allowsContentModifications } ?? cals.first
}

func registerCalendarHandlers(_ d: Dispatcher) {
    d.register("calendar.list_calendars") { _ in
        try EKStore.shared.ensureEvents()
        let cals = EKStore.shared.store.calendars(for: .event)
        let list: [[String: Any]] = cals.map {
            [
                "id": $0.calendarIdentifier,
                "title": $0.title,
                "source": $0.source?.title ?? "",
                "writable": $0.allowsContentModifications,
            ]
        }
        return ["calendars": list]
    }

    d.register("calendar.events_search") { req in
        try EKStore.shared.ensureEvents()
        let store = EKStore.shared.store
        let from = try D.optDate(req.params, "from") ?? Calendar.current.date(byAdding: .day, value: -7, to: Date())!
        let to = try D.optDate(req.params, "to") ?? Calendar.current.date(byAdding: .day, value: 14, to: Date())!
        let text = J.optStr(req.params, "text")?.lowercased()
        let calIds = J.optStrArray(req.params, "calendar_ids")
        let limit = J.optInt(req.params, "limit") ?? 50
        let calendars: [EKCalendar]? = calIds.isEmpty
            ? nil
            : store.calendars(for: .event).filter { calIds.contains($0.calendarIdentifier) }
        let pred = store.predicateForEvents(withStart: from, end: to, calendars: calendars)
        var events = store.events(matching: pred)
        events.sort { $0.startDate < $1.startDate }
        var out: [[String: Any]] = []
        var truncated = false
        for e in events {
            if let text {
                let hay = "\(e.title ?? "") \(e.notes ?? "") \(e.location ?? "")".lowercased()
                guard hay.contains(text) else { continue }
            }
            if out.count >= limit { truncated = true; break }
            out.append(eventDict(e))
        }
        return ["events": out, "truncated": truncated]
    }

    d.register("calendar.event_get") { req in
        try EKStore.shared.ensureEvents()
        let id = try J.reqStr(req.params, "id")
        guard let item = EKStore.shared.store.calendarItem(withIdentifier: id) as? EKEvent else {
            throw BridgeError.notFound("No event with id \(id)", app: "Calendar")
        }
        return ["event": eventDict(item)]
    }

    d.register("calendar.event_create") { req in
        try EKStore.shared.ensureEvents()
        let store = EKStore.shared.store
        let event = EKEvent(eventStore: store)
        event.title = try J.reqStr(req.params, "title")
        event.startDate = try D.reqDate(req.params, "start")
        if let end = try D.optDate(req.params, "end") {
            event.endDate = end
        } else if let durMin = J.optInt(req.params, "duration_minutes") {
            event.endDate = event.startDate.addingTimeInterval(TimeInterval(durMin * 60))
        } else {
            event.endDate = event.startDate.addingTimeInterval(3600)
        }
        if event.endDate < event.startDate {
            throw BridgeError.invalidParams("end must not be before start")
        }
        event.isAllDay = J.optBool(req.params, "all_day") ?? false
        if let v = J.optStr(req.params, "location") { event.location = v }
        if let v = J.optStr(req.params, "notes") { event.notes = v }
        if let v = J.optStr(req.params, "url") { event.url = URL(string: v) }
        if let calId = J.optStr(req.params, "calendar_id"),
           let cal = store.calendars(for: .event).first(where: { $0.calendarIdentifier == calId }) {
            event.calendar = cal
        } else {
            let cals = store.calendars(for: .event)
            event.calendar = cals.first { $0.allowsContentModifications } ?? cals.first
        }
        let relAlarms = J.optAnyArray(req.params, "alarms_minutes") as? [NSNumber] ?? []
        let absStrings = J.optStrArray(req.params, "alarm_times")
        if !relAlarms.isEmpty || !absStrings.isEmpty {
            var alarms = relAlarms.map { EKAlarm(relativeOffset: TimeInterval(-$0.intValue * 60)) }
            alarms += try absStrings.map { EKAlarm(absoluteDate: try D.parse($0)) }
            event.alarms = alarms
        }
        if let rec = J.optDict(req.params, "recurrence") {
            event.recurrenceRules = [try makeRecurrenceRule(rec)]
        }
        do {
            try store.save(event, span: .thisEvent)
        } catch {
            throw BridgeError(code: "internal", message: "Calendar save failed: \(error.localizedDescription)", app: "Calendar", fix: nil)
        }
        return ["event": eventDict(event)]
    }

    d.register("calendar.event_update") { req in
        try EKStore.shared.ensureEvents()
        let store = EKStore.shared.store
        let id = try J.reqStr(req.params, "id")
        guard let event = store.calendarItem(withIdentifier: id) as? EKEvent else {
            throw BridgeError.notFound("No event with id \(id)", app: "Calendar")
        }
        if let v = J.optStr(req.params, "title") { event.title = v }
        if let v = try D.optDate(req.params, "start") { event.startDate = v }
        if let v = try D.optDate(req.params, "end") { event.endDate = v }
        if let v = J.optBool(req.params, "all_day") { event.isAllDay = v }
        if event.endDate < event.startDate { event.endDate = event.startDate.addingTimeInterval(3600) }
        if let v = J.optStr(req.params, "location") { event.location = v }
        if let v = J.optStr(req.params, "notes") { event.notes = v }
        if let v = J.optStr(req.params, "url") { event.url = URL(string: v) }
        if let calId = J.optStr(req.params, "calendar_id"),
           let cal = store.calendars(for: .event).first(where: { $0.calendarIdentifier == calId }) {
            event.calendar = cal
        }
        if req.params.keys.contains("alarms_minutes") || req.params.keys.contains("alarm_times") {
            var alarms: [EKAlarm] = []
            let rel = J.optAnyArray(req.params, "alarms_minutes") as? [NSNumber] ?? []
            alarms += rel.map { EKAlarm(relativeOffset: TimeInterval(-$0.intValue * 60)) }
            let abs = J.optStrArray(req.params, "alarm_times")
            alarms += try abs.map { EKAlarm(absoluteDate: try D.parse($0)) }
            event.alarms = alarms
        }
        if let rec = J.optDict(req.params, "recurrence") {
            event.recurrenceRules = [try makeRecurrenceRule(rec)]
        }
        try store.save(event, span: .thisEvent)
        return ["event": eventDict(event)]
    }

    d.register("calendar.event_delete") { req in
        try EKStore.shared.ensureEvents()
        let store = EKStore.shared.store
        let id = try J.reqStr(req.params, "id")
        guard let event = store.calendarItem(withIdentifier: id) as? EKEvent else {
            throw BridgeError.notFound("No event with id \(id)", app: "Calendar")
        }
        let preview = eventDict(event)
        guard J.optBool(req.params, "confirm") == true else {
            return ["would_delete": preview, "confirm_required": true]
        }
        try store.remove(event, span: .thisEvent)
        return ["deleted": true, "id": id]
    }

    d.register("calendar.availability") { req in
        try EKStore.shared.ensureEvents()
        let store = EKStore.shared.store
        let from = try D.reqDate(req.params, "from")
        let to = try D.reqDate(req.params, "to")
        guard from < to else { throw BridgeError.invalidParams("from must be before to") }
        let calIds = J.optStrArray(req.params, "calendar_ids")
        let calendars: [EKCalendar]? = calIds.isEmpty
            ? nil
            : store.calendars(for: .event).filter { calIds.contains($0.calendarIdentifier) }
        let pred = store.predicateForEvents(withStart: from, end: to, calendars: calendars)
        let events = store.events(matching: pred)
            .filter { !$0.isAllDay && $0.status != .canceled }
            .sorted { $0.startDate < $1.startDate }

        var busyRanges: [(Date, Date)] = []
        var mergedStart: Date?
        var mergedEnd: Date?
        for e in events {
            let s = max(e.startDate, from)
            let en = min(e.endDate, to)
            guard s < en else { continue }
            if let cs = mergedStart, let ce = mergedEnd, s <= ce {
                mergedEnd = max(ce, en)
            } else {
                if let cs = mergedStart, let ce = mergedEnd {
                    busyRanges.append((cs, ce))
                }
                mergedStart = s
                mergedEnd = en
            }
        }
        if let cs = mergedStart, let ce = mergedEnd {
            busyRanges.append((cs, ce))
        }
        let busy: [[String: Any]] = busyRanges.map {
            ["start": D.format($0.0) ?? "", "end": D.format($0.1) ?? ""]
        }

        var free: [[String: Any]] = []
        var cursor = from
        for (bs, be) in busyRanges {
            if cursor < bs { free.append(["start": D.format(cursor) ?? "", "end": D.format(bs) ?? ""]) }
            cursor = max(cursor, be)
        }
        if cursor < to { free.append(["start": D.format(cursor) ?? "", "end": D.format(to) ?? ""]) }
        return ["busy": busy, "free": free]
    }
}

// MARK: - Reminders

func reminderDict(_ r: EKReminder) -> [String: Any] {
    var d: [String: Any] = [
        "id": r.calendarItemIdentifier,
        "list_id": r.calendar.calendarIdentifier,
        "list": r.calendar.title,
        "title": r.title ?? "",
        "completed": r.isCompleted,
        "priority": r.priority,
    ]
    if let v = r.notes, !v.isEmpty { d["notes"] = v }
    if let v = r.url { d["url"] = v.absoluteString }
    if let comps = r.dueDateComponents, let due = comps.date as Date? {
        d["due"] = D.format(due) ?? ""
        d["due_has_time"] = comps.hour != nil
    }
    if let v = D.format(r.completionDate) { d["completed_at"] = v }
    let alarms = r.alarms?.compactMap { $0.absoluteDate != nil ? D.format($0.absoluteDate!) : nil } ?? []
    if !alarms.isEmpty { d["alarms"] = alarms }
    return d
}

func pickReminderList(store: EKEventStore, id: String?, title: String?) -> EKCalendar? {
    let lists = store.calendars(for: .reminder)
    if let id { return lists.first { $0.calendarIdentifier == id } }
    if let title { return lists.first { $0.title.caseInsensitiveCompare(title) == .orderedSame } }
    return lists.first
}

func registerRemindersHandlers(_ d: Dispatcher) {
    d.register("reminders.list_lists") { _ in
        try EKStore.shared.ensureReminders()
        let lists = EKStore.shared.store.calendars(for: .reminder)
        let out: [[String: Any]] = lists.map {
            ["id": $0.calendarIdentifier, "title": $0.title, "source": $0.source?.title ?? ""]
        }
        return ["lists": out]
    }

    d.register("reminders.search") { req in
        try EKStore.shared.ensureReminders()
        let store = EKStore.shared.store
        let listId = J.optStr(req.params, "list_id")
        let listTitle = J.optStr(req.params, "list")
        let lists: [EKCalendar]? = (listId == nil && listTitle == nil)
            ? nil
            : [pickReminderList(store: store, id: listId, title: listTitle)].compactMap { $0 }
        let pred = store.predicateForReminders(in: lists)
        var reminders: [EKReminder] = []
        let sem = DispatchSemaphore(value: 0)
        store.fetchReminders(matching: pred) { items in
            reminders = items ?? []
            sem.signal()
        }
        sem.wait()

        let text = J.optStr(req.params, "text")?.lowercased()
        let completed = J.optBool(req.params, "completed")
        let dueWithinDays = J.optInt(req.params, "due_within_days")
        let limit = J.optInt(req.params, "limit") ?? 100

        func dueDate(_ r: EKReminder) -> Date {
            r.dueDateComponents?.date ?? .distantFuture
        }
        let ordered = reminders.sorted { dueDate($0) < dueDate($1) }

        var out: [[String: Any]] = []
        var truncated = false
        let now = Date()
        for r in ordered {
            if let completed, r.isCompleted != completed { continue }
            if let text {
                let hay = "\(r.title ?? "") \(r.notes ?? "")".lowercased()
                guard hay.contains(text) else { continue }
            }
            if let days = dueWithinDays {
                guard let due = r.dueDateComponents?.date as Date?, !r.isCompleted,
                      due >= now, due <= now.addingTimeInterval(TimeInterval(days) * 86400)
                else { continue }
            }
            if out.count >= limit { truncated = true; break }
            out.append(reminderDict(r))
        }
        return ["reminders": out, "truncated": truncated]
    }

    d.register("reminders.create") { req in
        try EKStore.shared.ensureReminders()
        let store = EKStore.shared.store
        let reminder = EKReminder(eventStore: store)
        reminder.title = try J.reqStr(req.params, "title")
        if let v = J.optStr(req.params, "notes") { reminder.notes = v }
        if let v = J.optStr(req.params, "url"), let url = URL(string: v) { reminder.url = url }
        if let prio = J.optInt(req.params, "priority") {
            guard (1...9).contains(prio) else { throw BridgeError.invalidParams("priority must be 1-9") }
            reminder.priority = prio
        }
        if let due = try D.optDate(req.params, "due") {
            let hasTime = J.optBool(req.params, "due_has_time") ?? true
            var comps = Calendar.current.dateComponents([.year, .month, .day], from: due)
            if hasTime { comps = Calendar.current.dateComponents([.year, .month, .day, .hour, .minute], from: due) }
            comps.calendar = Calendar.current
            reminder.dueDateComponents = comps
        }
        if let alarm = try D.optDate(req.params, "alarm_time") {
            reminder.alarms = [EKAlarm(absoluteDate: alarm)]
        }
        reminder.calendar = pickReminderList(store: store, id: J.optStr(req.params, "list_id"), title: J.optStr(req.params, "list"))
            ?? pickReminderList(store: store, id: nil, title: nil)
        try store.save(reminder, commit: true)
        return ["reminder": reminderDict(reminder)]
    }

    d.register("reminders.update") { req in
        try EKStore.shared.ensureReminders()
        let store = EKStore.shared.store
        let id = try J.reqStr(req.params, "id")
        guard let reminder = store.calendarItem(withIdentifier: id) as? EKReminder else {
            throw BridgeError.notFound("No reminder with id \(id)", app: "Reminders")
        }
        if let v = J.optStr(req.params, "title") { reminder.title = v }
        if let v = J.optStr(req.params, "notes") { reminder.notes = v }
        if let v = J.optStr(req.params, "url"), let url = URL(string: v) { reminder.url = url }
        if let prio = J.optInt(req.params, "priority") {
            guard (1...9).contains(prio) else { throw BridgeError.invalidParams("priority must be 1-9") }
            reminder.priority = prio
        }
        if let completed = J.optBool(req.params, "completed") {
            reminder.isCompleted = completed
            reminder.completionDate = completed ? Date() : nil
        }
        if req.params.keys.contains("due") {
            if let due = try D.optDate(req.params, "due") {
                let hasTime = J.optBool(req.params, "due_has_time") ?? true
                var comps = Calendar.current.dateComponents(hasTime ? [.year, .month, .day, .hour, .minute] : [.year, .month, .day], from: due)
                comps.calendar = Calendar.current
                reminder.dueDateComponents = comps
            } else {
                reminder.dueDateComponents = nil
            }
        }
        if req.params.keys.contains("alarm_time") {
            if let alarm = try D.optDate(req.params, "alarm_time") {
                reminder.alarms = [EKAlarm(absoluteDate: alarm)]
            } else {
                reminder.alarms = nil
            }
        }
        if let listId = J.optStr(req.params, "list_id") ?? J.optStr(req.params, "list"),
           let list = pickReminderList(store: store, id: J.optStr(req.params, "list_id"), title: J.optStr(req.params, "list")) {
            reminder.calendar = list
        }
        try store.save(reminder, commit: true)
        return ["reminder": reminderDict(reminder)]
    }

    d.register("reminders.delete") { req in
        try EKStore.shared.ensureReminders()
        let store = EKStore.shared.store
        let id = try J.reqStr(req.params, "id")
        guard let reminder = store.calendarItem(withIdentifier: id) as? EKReminder else {
            throw BridgeError.notFound("No reminder with id \(id)", app: "Reminders")
        }
        let preview = reminderDict(reminder)
        guard J.optBool(req.params, "confirm") == true else {
            return ["would_delete": preview, "confirm_required": true]
        }
        try store.remove(reminder, commit: true)
        return ["deleted": true, "id": id]
    }
}
