import Foundation

/// Date parsing/formatting for the bridge protocol.
/// - Input:  ISO 8601 with offset ("2026-09-21T15:00:00+02:00", Z accepted),
///           or "yyyy-MM-dd" (interpreted as local midnight, i.e. an all-day anchor).
/// - Output: always ISO 8601 with the local UTC offset.
enum D {
    private static func formatter(_ format: String) -> DateFormatter {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = format
        return f
    }

    private static let isoParsers: [ISO8601DateFormatter] = {
        let base = ISO8601DateFormatter()
        base.formatOptions = [.withInternetDateTime]
        let frac = ISO8601DateFormatter()
        frac.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return [base, frac]
    }()

    private static let dayParser = formatter("yyyy-MM-dd")

    static func parse(_ s: String) throws -> Date {
        for f in isoParsers {
            if let d = f.date(from: s) { return d }
        }
        if let d = dayParser.date(from: s) { return d }
        throw BridgeError.invalidParams("cannot parse date '\(s)' (expected ISO 8601, e.g. 2026-09-21T15:00:00+02:00)")
    }

    static func optDate(_ p: [String: Any], _ key: String) throws -> Date? {
        guard let s = J.optStr(p, key) else { return nil }
        return try parse(s)
    }

    static func reqDate(_ p: [String: Any], _ key: String) throws -> Date {
        guard let s = J.optStr(p, key) else {
            throw BridgeError.invalidParams("missing required date field '\(key)'")
        }
        return try parse(s)
    }

    private static let outFormatter: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime]
        return f
    }()

    static func format(_ date: Date?) -> String? {
        guard let date else { return nil }
        return outFormatter.string(from: date)
    }

    static func optDay(_ p: [String: Any], _ key: String) throws -> Date? {
        guard let s = J.optStr(p, key) else { return nil }
        guard let d = dayParser.date(from: s) else {
            throw BridgeError.invalidParams("field '\(key)' must be yyyy-MM-dd")
        }
        return d
    }
}
