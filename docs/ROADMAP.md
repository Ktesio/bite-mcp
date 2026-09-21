# Roadmap

## Phase 1 — shipped (Core PIM)

Calendar, Reminders (EventKit) · Contacts (Contacts.framework) · Mail, Notes,
Messages (ScriptingBridge) · 38 MCP tools · CLI twins · setup/doctor ·
marketplace plugin · config writers for 7 agent CLIs.

## Phase 2 — planned

### Apps
- **Music** — play/pause/queue/search library, playlists (ScriptingBridge).
- **Finder** — file move/reveal/tagging on Desktop & Documents.
- **Shortcuts** — run any user shortcut as a tool (`shortcuts run`) — huge
  multiplier: users can extend bite with their own automations.
- **Safari** — reading list, open tabs, history (where scriptable).

### Depth
- **Generic ScriptingBridge command bridge**: one `app_script` tool taking
  `{app, command, params}` that maps to any sdef verb — full "every
  functionality" coverage for power users, with the typed tools remaining the
  guided path.
- **Calendar attendees/invites** — EventKit cannot create participants;
  investigate EventKitKit + CalendarStore paths or document as unsupported.
- **Mail forward with original attachments**; rules management.
- **Location-based reminder alarms** (needs When-In-Use location permission —
  a new TCC family; ship behind a config flag).
- **Reminder recurrence editing**, list create/delete.

### Platform
- **MCP resource subscriptions** — EventKit's `EKEventStoreChanged` forwarded
  as MCP `notifications/resources/updated`; live calendar/mailbox state.
- **Notarized prebuilt helper channel** — `bite install-helper --prebuilt`
  downloads the release artifact (for users without Xcode CLT).
- **Homebrew formula + tap**; npm shim for `npx`-style launchers.
- **Pagination polish for huge mailboxes** (whose-clause Apple Events via
  hand-built descriptors for the hot search path).
- **Per-client E2E fixtures** — golden conversations replayed for each of the
  7 config writers.

## Non-goals

- No osascript/AppleScript strings anywhere — native APIs only.
- No non-Apple email/calendar backends; bite is deliberately local-only.
- No silent destructive actions — confirm-first stays forever.
