import Foundation
import EventKit
import Contacts

/// System-level handlers: health, version, and permission probes.
///
/// Probes are deliberately prompt-free: they only read TCC status enums.
/// Apple Events permissions (Mail/Notes/Messages) cannot be queried without
/// sending an event, so they report "will_prompt_on_first_use" unless the
/// caller passes `probe: true` — doctor does that only with --probe.
enum SysBridge {
    static func register(_ d: Dispatcher) {
        d.register("sys.ping") { _ in
            ["pong": true, "protocol": PROTOCOL_VERSION, "version": HELPER_VERSION]
        }

        d.register("sys.version") { _ in
            [
                "version": HELPER_VERSION,
                "protocol": PROTOCOL_VERSION,
                "os": ProcessInfo.processInfo.operatingSystemVersionString,
                "capabilities": CAPABILITIES,
            ]
        }

        d.register("sys.probe") { req in
            let app = J.optStr(req.params, "app") ?? ""
            let liveProbe = J.optBool(req.params, "probe") ?? false
            switch app {
            case "calendar":
                return probeEventkit(.event)
            case "reminders":
                return probeEventkit(.reminder)
            case "contacts":
                let status = CNContactStore.authorizationStatus(for: .contacts)
                let state: String
                switch status {
                case .authorized: state = "authorized"
                case .denied: state = "denied"
                case .restricted: state = "restricted"
                case .notDetermined: state = "not_determined"
                @unknown default: state = "unknown"
                }
                return ["state": state, "prompts_on_use": state == "not_determined"]
            case "mail", "notes", "messages":
                // Apple Events (kTCCServiceAppleEvents) status is not queryable
                // without sending an event, which itself triggers the prompt.
                if liveProbe {
                    return probeAppleEvents(app: app)
                }
                return ["state": "will_prompt_on_first_use", "prompts_on_use": true]
            default:
                throw BridgeError.invalidParams("unknown app '\(app)' (calendar|reminders|contacts|mail|notes|messages)")
            }
        }
    }

    private static func probeEventkit(_ type: EKEntityType) -> [String: Any] {
        let status = EKEventStore.authorizationStatus(for: type)
        let state: String
        switch status {
        case .authorized, .fullAccess:
            state = "authorized"
        case .writeOnly:
            state = "write_only"
        case .denied: state = "denied"
        case .restricted: state = "restricted"
        case .notDetermined: state = "not_determined"
        @unknown default: state = "unknown"
        }
        return ["state": state, "prompts_on_use": state == "not_determined"]
    }

    private static func probeAppleEvents(app: String) -> [String: Any] {
        let bundle: String
        let name: String
        switch app {
        case "mail": bundle = MAIL_BUNDLE; name = "Mail"
        case "notes": bundle = NOTES_BUNDLE; name = "Notes"
        default: bundle = MESSAGES_BUNDLE; name = "Messages"
        }
        do {
            let sb = try sbApp(bundleID: bundle, name: name)
            _ = sbStr(sb, "name")  // one harmless Apple Event — this is where TCC judges us
            return ["state": "authorized", "prompts_on_use": false]
        } catch let e as BridgeError where e.code == "app_not_running" {
            return ["state": "app_missing", "prompts_on_use": false]
        } catch {
            return ["state": "denied_or_unavailable", "prompts_on_use": false]
        }
    }
}
