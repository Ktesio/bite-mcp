# bite

**Native Apple apps for AI agents** — Calendar, Reminders, Mail, Notes,
Contacts and Messages over MCP. Rust control plane, native Swift helper
(EventKit + ScriptingBridge), plus a detached Swift crawl worker that
compiles AppleScript through NSAppleScript (OSA) to index Mail. No
`osascript` processes, no Python.

> **Disclaimer:** bite is an independent open-source project. It is **not
> affiliated with, sponsored, or endorsed by Apple Inc.** macOS, iMessage,
> iCloud, Calendar, Mail, Notes, Messages, Contacts and other Apple marks are
> trademarks of Apple Inc.

```bash
cargo install bite-mcp
bite setup
```

Then in Claude Code or ZCode:

```
/plugin marketplace add ktesio/bite-mcp
/plugin install bite@bite-mcp
```

…or let `bite setup` write MCP config directly into Codex, OpenCode, Cursor,
VS Code, Gemini CLI (whichever it detects).

## What you get

45 MCP tools + mirrored CLI verbs:

| App | Engine | Tools |
|-----|--------|-------|
| Calendar | EventKit (native) | list, search, get, create, update, delete, availability/free-busy |
| Reminders | EventKit (native) | lists, search, create (alarms), update/complete, delete |
| Mail | ScriptingBridge | accounts, mailboxes, search, get, send, reply, forward, move, mark, delete, attachment save |
| Notes | ScriptingBridge | folders, search, get (markdown+HTML), create, update (append), delete |
| Contacts | Contacts.framework (native) | search, get, create, update, delete, groups |
| Messages | ScriptingBridge + chat.db | send, recent chats, history |
| Index | LanceDB + `bite-crawl` worker | index_rebuild, index_status, index_crawl_cancel, index_wipe, search, mail_bulk_* |

Plus MCP resources (`bite://calendars`, `bite://mail/mailboxes`, …) and
prompts (`plan-my-week`, `triage-inbox`, `daily-brief`).

Every tool is also a human CLI verb:

```bash
bite reminders lists
bite reminders create "Water the plants" --due "tomorrow 9am"
bite calendar availability --from "today 9am" --to "today 6pm"
bite calendar create-event --title "Standup" --start "tomorrow 10am" --duration-minutes 15
bite mail search --mailbox INBOX --unread --limit 10
bite notes create --title "Ideas" --body "# Idea\n- **thing**"
```

## Performance

Bite indexes your mail into its own LanceDB database (in bite's data dir,
mode 0700) and answers searches locally — no Apple Events on the query path.

| Operation (124k-message mailbox) | bite | Typical per-message AppleScript MCPs |
|---|---|---|
| Index/ingest 100k messages | 7.1 s one-time | n/a (never completes) |
| Full-text search | 8–10 ms | 120 s+ timeout at 100 messages |
| Exact count (66k unread) | 8.9 ms | times out |
| Bulk mark-unread-read | a handful of Apple Events total | 1,834+ events |

Reproduce: `cargo run -p bite-index --example bench_100k --release`.
Bulk ops (mark/move/delete, whole mailbox or filtered) likewise run as a
whose-clause script per operation plus verification counts — a handful of
Apple Events total, still O(1) vs per-message: Mail iterates internally
while bite waits.

Mail crawl throughput depends on Mail's own Apple Event latency; the
index-side numbers above are the LanceDB bench.

## Why this architecture

```
Agent / human
     │
bite (Rust): MCP stdio server · CLI · setup · doctor · config writers
     │  line-delimited JSON (docs/protocol.md)
bite-helper (Swift, installed once at a stable path):
     ├─ EventKit            → Calendar, Reminders   (full CRUD)
     ├─ Contacts.framework  → Contacts              (full CRUD)
     └─ ScriptingBridge     → Mail, Notes, Messages (Apple Events, no osascript)

bite (Rust) also spawns a background worker for the Mail index:
bite-crawl (Swift, detached worker): Mail crawl → staged JSONL batches
     │  batches/*.jsonl
     ▼
bite (Rust) ingests them into the entity index (LanceDB) — instant local search
```

- **Native, not `osascript`.** Every call goes through Apple's own APIs
  (EventKit, Contacts.framework, ScriptingBridge — and NSAppleScript for the
  Mail crawl worker).
- **OSA-mediated Apple Events.** From macOS 27, raw (non-OSA) Apple Event
  senders get silent empty replies. Every bite component that sends Apple
  Events goes through OSA — the helper via ScriptingBridge, the Mail crawl
  worker via NSAppleScript — so events are answered normally; TCC behavior
  is unchanged.
- **Stable helper path** (`~/Library/Application Support/bite/bin/`) keeps
  macOS TCC permission grants valid across upgrades.
- **One registry, two faces.** MCP `tools/list` schemas and CLI verbs are
  generated from the same table (`crates/bite-core/src/registry.rs`); a CI
  conformance test fails if Swift handlers and the Rust registry drift apart.
- **Agent-friendly errors.** Permission denials return an exact `fix` — a
  command or a System Settings deep link the agent relays verbatim.
- **Confirm-before-destruct.** Deletes/moves return a preview until the caller
  passes `confirm: true`.

## Install paths

| Path | Command | Needs |
|------|---------|-------|
| crates.io (source) | `cargo install bite-mcp` | Xcode Command Line Tools (compiles both Swift binaries — helper + crawl worker) |
| marketplace (Claude/ZCode) | `/plugin marketplace add ktesio/bite-mcp` | `bite` on PATH first |
| other agent CLIs | `bite setup` (writes their MCP config) | `bite` on PATH |

## First run

1. `bite setup` — installs the helper, runs `bite doctor`, configures detected
   agent CLIs, prints marketplace commands.
2. First call per app shows macOS's own permission prompt once (Calendar,
   Reminders, Contacts, and one "wants to control Mail" prompt per SB app).
   The crawl worker is a separate binary with its own TCC identity, so Mail
   indexing raises one more "wants to control Mail" prompt of its own — see
   [docs/permissions.md](docs/permissions.md).
3. Denied something? `bite doctor --fix` opens the right System Settings panes.

## Privacy & intended use

- **Local-only.** bite runs entirely on your Mac. It makes no network calls
  beyond what macOS itself requires, collects no telemetry, and has no
  accounts or servers — your mail, messages, contacts, calendar events and
  notes never leave the device.
- **Your data, your grants.** Access happens only through macOS's own
  permission system (TCC), with visible, one-time prompts. Every grant is
  visible and revocable in System Settings → Privacy & Security — see
  [docs/permissions.md](docs/permissions.md).
- **Own-device tool.** bite is designed to operate on the data of the person
  running it, on their own machine, with their knowledge. **Acceptable use:**
  do not use bite to access another person's device or accounts, or to monitor
  anyone without their consent — that is unlawful in most jurisdictions and
  outside the purpose of this project.
- Full statement: [PRIVACY.md](PRIVACY.md).

## Documentation

- [PRIVACY.md](PRIVACY.md) — privacy statement and intended use
- [NOTICE.md](NOTICE.md) — attribution and trademark notices
- [SECURITY.md](SECURITY.md) — how to report security issues
- [docs/PLAN.md](docs/PLAN.md) — the full implementation plan
- [docs/protocol.md](docs/protocol.md) — the Rust↔Swift helper protocol and
  the crawl batch handoff
- [docs/permissions.md](docs/permissions.md) — every TCC prompt and how to recover
- [docs/adding-an-app.md](docs/adding-an-app.md) — add Music/Finder/anything
- [docs/ROADMAP.md](docs/ROADMAP.md) — Phase 2 surface

## License

MIT — see [LICENSE](LICENSE) and [NOTICE.md](NOTICE.md).
