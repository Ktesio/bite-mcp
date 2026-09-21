# bite-mcp — Implementation Plan (as built)

**Repo:** `~/dev/bite-mcp`. One published crate `bite-mcp` → binary `bite`.
Native Apple integration lives entirely in a Swift helper (EventKit +
ScriptingBridge + Contacts.framework); Rust is the control plane (CLI, MCP
stdio server, setup/doctor, distribution).

## 1. Architecture (as implemented)

```
Agent CLI (Claude/Codex/ZCode/OpenCode/…)   Human / script
        │ stdio MCP (NDJSON JSON-RPC 2.0)       │ argv
        ▼                                       ▼
┌────────────────────── bite (Rust) ──────────────────────┐
│  bite mcp → MCP server (hand-rolled, spec-stable)      │
│  bite <app> <verb> · bite setup · doctor · config      │
│                bite-bridge (spawn + NDJSON protocol)   │
└──────────────────────┬─────────────────────────────────┘
                       │ long-lived child, NDJSON over stdio
                       ▼
   bite-helper (Swift, ~/Library/Application Support/bite/bin/)
     ├─ EventKit: Calendar + Reminders   (TCC Calendars/Reminders)
     ├─ Contacts.framework: Contacts     (TCC Contacts)
     └─ ScriptingBridge: Mail, Notes, Messages (TCC Apple Events, per app)
```

Deviations from the original plan, with reasons:
- **MCP server is hand-rolled** instead of rmcp: the stdio surface of MCP is
  small and stable; this removes SDK API churn and a dependency tree. All
  clients work (verified with an end-to-end smoke test and MCP Inspector in CI).
- **Swift package lives at `crates/bite-mcp/swift/`** so `cargo publish`
  ships the source with the crate (include paths cannot escape the crate root).
- **No async runtime**: everything is synchronous threads; MCP stdio is a
  sequential request/response protocol.

## 2. Key decisions

- **Zero `osascript` anywhere.** EventKit APIs, `SBApplication` via dynamic
  selectors (no generated sdef headers — resilient across macOS versions),
  `CNContactStore`, and a read-only sqlite fallback for Messages history.
- **Helper installed once** to `~/Library/Application Support/bite/bin/` so
  TCC grants survive rebuilds; `bite install-helper --force` refreshes.
- **Helper runs on one serial queue.** All Apple work is thread-confined.
  (A main-queue hop was tried and deadlocks — see main.swift comment.)
- **One core layer, two faces:** `bite-core` registry is the single table of
  38 tools (name, bridge method, params, JSON schema). MCP and CLI both
  funnel through `ops::run`. A CI test asserts every registry method exists in
  the Swift dispatcher and no stray handlers exist.
- **Agent-friendly errors:** every failure carries `{code, message, app, fix}`
  where `fix` is actionable (`open 'x-apple.systempreferences:…'`, …).
- **Confirm-before-destruct:** delete/move tools return a preview until
  `confirm: true`.

## 3. Crates

- `bite-bridge` — protocol types, spawn/lifecycle (handshake, correlation,
  timeouts, crash handling), scripted `bite-fake-helper` for golden tests.
- `bite-core` — registry (38 tools), config (TOML in app-data dir), dates
  (strict ISO for MCP + relaxed parser for CLI: "tomorrow 3pm", "in 2h"),
  error rendering.
- `bite-mcp` (bin `bite`) — clap CLI mirroring every tool; `mcp serve`
  (initialize/tools/resources/prompts/ping); `setup`; `doctor [--fix|--probe]`;
  `install-helper`; `helper ping|version|raw`; config writers for 7 agent
  clients (Claude, ZCode, Codex, OpenCode, Cursor, VS Code, Gemini CLI) with
  surgical read-modify-write (`.bite-bak` backup, `toml_edit` for Codex).
- `build.rs` — compiles the embedded Swift package at build time
  (content-hash gated), exposes `BITE_HELPER_BUILT`; the binary also
  compiles-on-demand from the packaged sources when the artifact is absent.

## 4. MCP tool surface (38 tools)

calendar: list_calendars, events_search, event_get, event_create, event_update, event_delete*, event_availability ·
reminders: list_lists, search, create, update, delete* ·
mail: accounts, mailboxes_list, messages_search, message_get, send, reply, forward, move*, mark, delete*, attachment_save ·
notes: folders, search, get, create, update, delete* ·
contacts: search, get, create, update, delete*, groups ·
messages: send, chats_recent, history
(* = confirm-first)

Resources: `bite://calendars`, `bite://reminders/lists`,
`bite://mail/mailboxes`, `bite://notes/folders`, `bite://contacts/groups`.
Prompts: `plan-my-week`, `triage-inbox`, `daily-brief`.

## 5. Milestones (all complete)

1. **Scaffold** — workspace, CI, README, protocol spec, crates.io name check ✓
2. **Bridge runtime** — protocol + lifecycle + fake-helper golden tests ✓
3. **Calendar + Reminders vertical slice** (EventKit) ✓
4. **MCP server + setup/doctor/config writers** ✓
5. **Mail + Notes** (dynamic ScriptingBridge, markdown↔HTML) ✓
6. **Contacts + Messages** (Contacts.framework; chat.db FDA fallback) ✓
7. **Marketplace plugin + skill** ✓
8. **Release engineering** ✓ — source-only releases: tag → build + test →
   GitHub Release notes → crates.io publish. Notarized prebuilts deliberately
   deferred (Apple Developer Program would be required; ROADMAP).
9. **Hardening** — error-actionability, scan caps with honest `truncated`
   flags, watchdogs on Apple Event calls ✓

## 6. Risks & mitigations (as handled)

- **TCC Apple-Events re-prompts on rebuild** → stable install path +
  version-gated refresh; source-compile path immune to Gatekeeper.
- **Silent AE denial in non-interactive shells** (observed during development):
  SB returns nil/errors; doctor probes report state without prompting.
- **Messages history needs Full Disk Access** → feature-detect chat.db,
  degrade to ScriptingBridge-only data, doctor surfaces it.
- **SBElementArray indexing ambiguity** → all access funnels through `sbAt`
  (one place to flip) + documented convention.
- **Notes/Mail HTML-only bodies** → markdown converters with unit tests.
- **Mailbox scans are Apple-Event-per-message** → newest-first walk, `limit`
  + `max_scan` caps, honest `truncated` flag.

## Phase 2 (planned, not in this scope)

Music, Finder, Shortcuts, Safari; generic ScriptingBridge command bridge
(`{app, command, params}` exposing any sdef verb); MCP resource subscriptions
for change push; location-based reminder alarms (CL permission); calendar
attendee invites (EventKit limitation); mail forward with original
attachments.
