# Bridge & handoff protocols (Rust ↔ Swift)

Part 1 is the helper protocol: line-delimited JSON (NDJSON) over the helper's
stdin/stdout. One JSON value per line, UTF-8, `\n` terminated. Helper stderr
is free-form diagnostics (mirrored by the Rust side to its own stderr). The
staged batch handoff at the end is a separate, file-based contract with the
detached `bite-crawl` worker — no NDJSON stream, no handshake.

Version constant: `PROTOCOL_VERSION = 1` (both sides). Handshake mismatch is
a fatal spawn error telling the user to run `bite install-helper --force`.

## Handshake

The helper's first stdout line, unrequested:

```json
{"method":"hello","params":{"protocol":1,"version":"0.1.0","capabilities":["calendar","reminders","contacts","mail","notes","messages"]}}
```

The Rust side waits up to 15 s, verifies `protocol == 1`, and verifies the
process is still alive.

## Requests

```json
{"id":7,"method":"calendar.event_create","params":{"title":"Lunch","start":"2026-09-22T12:00:00+02:00"}}
```

- `id`: integer, strictly increasing, correlated by the Rust client.
- `method`: `<namespace>.<verb>`; namespaces: `sys`, `calendar`, `reminders`,
  `contacts`, `mail`, `notes`, `messages`.
- `params`: object; absent/optional fields may be omitted. Dates are
  ISO 8601 with offset; `yyyy-MM-dd` means an all-day anchor (local midnight).

## Responses

Success:

```json
{"id":7,"result":{"event":{"id":"…","title":"Lunch","start":"…","end":"…"}}}
```

Failure:

```json
{"id":7,"error":{"code":"permission_denied","message":"Access to Reminders was denied…","app":"Reminders","fix":"open 'x-apple.systempreferences:com.apple.preference.security?Privacy_Reminders'"}}
```

Notifications (no `id`) never receive a response; `log` notifications are
mirrored to stderr.

## Error codes

| code | meaning | typical fix |
|------|---------|-------------|
| `permission_denied` | TCC denied or not yet granted | System Settings deep link |
| `fda_required` | needs Full Disk Access (chat.db) | Settings → Full Disk Access |
| `not_found` | id/mailbox/folder/attachment missing | check earlier search results |
| `invalid_params` | schema violation | fix the call |
| `app_not_running` / `app_missing` | target app missing | install/launch the app |
| `timeout` | Apple Event call exceeded watchdog | retry |
| `internal` | unexpected failure | see message |

## Method table

sys: `sys.ping` `{"pong":true,…}` · `sys.version` · `sys.probe {app, probe?}`
(prompt-free TCC status probe; `probe:true` live-tests Apple Events and
triggers prompts)

calendar: `list_calendars` · `events_search {from?, to?, calendar_ids?, text?, limit?}` ·
`event_get {id}` · `event_create {title, start, end?/duration_minutes?, all_day?, calendar_id?, location?, notes?, url?, alarms_minutes?, alarm_times?, recurrence?}` ·
`event_update {id, …fields}` · `event_delete {id, confirm?}` (returns `would_delete` preview when confirm missing) ·
`availability {from, to, calendar_ids?}` → busy (merged) + free intervals

reminders: `list_lists` · `search {list?/list_id?, text?, completed?, due_within_days?, limit?}` ·
`create {title, list?, notes?, url?, priority?, due?, due_has_time?, alarm_time?}` ·
`update {id, …}` · `delete {id, confirm?}`

contacts: `search {query, limit?}` · `get {id}` · `create {first?, last?, org?, note?, emails?, phones?, urls?}` ·
`update {id, …}` · `delete {id, confirm?}` · `groups {}`

mail: `accounts` · `mailboxes {account?}` · `messages_search {mailbox?=INBOX, account?, from?, to?, subject?, body?, unread?, flagged?, since?, until?, limit?=20, max_scan?=400}` (newest-first walk, honest `truncated`) ·
`message_get {id, mailbox?, account?}` · `send {to[], cc?, bcc?, subject, body?, html?, account?, attachments?}` ·
`reply {id, mailbox?, reply_all?, body?, html?}` · `forward {id, to[], …}` ·
`move {id, to_mailbox, mailbox?, confirm?}` · `mark {id, read?, flagged?, junk?}` ·
`delete {id, mailbox?, confirm?}` · `attachment_save {id, index?, dir?}`

notes: `folders` · `search {text?, folder?, limit?}` · `get {id}` (HTML + markdown) ·
`create {title?, body? (markdown), folder?}` · `update {id, title?, body?, append?, folder?}` ·
`delete {id, confirm?}`

messages: `send {to, text}` (recipient must exist in Messages) ·
`chats_recent {limit?}` · `history {chat_id, limit?}` (source: `messages_app` or `chat.db`)

## Result shapes (contract)

Entity dicts are flat and snake_case. Canonical keys: event
(`id, calendar_id, calendar, title, start, end, all_day, location?, notes?, url?, recurrence?, status?`),
reminder (`id, list_id, list, title, notes?, due?, due_has_time, completed, priority, url?, alarms?`),
contact (`id, first, last, org?, emails?, phones?, urls?, note?, birthday?`),
mail message (`id, mailbox, subject, from, to?, date, read, flagged, snippet?, content?, attachments?`),
note (`id, title, folder?, body?, markdown?, created?, updated?`),
chat (`id, name?, participants?, last_message?`), message (`id, text, from, time?`).

Both sides are pinned by tests: `tests/protocol_conformance.rs` (Rust) and
`ProtocolTests` (Swift) plus the fake-helper golden conversations.

## Staged batch handoff (crawl worker → index)

Mail search is served from a local entity index (LanceDB). The detached
`bite-crawl` worker owns Mail Apple Events *for indexing and bulk
operations*; the Rust control plane itself never sends Mail Apple Events.
Live `mail.*` tools still go through the helper's ScriptingBridge —
including the live fallback while the index has no Mail rows yet
(`force_live`, `index_not_ready`). The two sides meet on disk in bite's data
dir.

### crawl-state.json

The worker's progress file, shared by crawl and bulk jobs (bulk job ids look
like `bulk-<unix time>`; a bulk job overwrites the previous job's terminal
state and refreshes `updated_at`). Keys: `job_id`, `state`, `processed`,
`found`, `window` (diagnostic label), `updated_at` (ISO 8601), `failures`
(consecutive failed crawl jobs — drives respawn backoff), `skipped`
(cumulative skipped mailboxes). States:

| state | meaning |
|-------|---------|
| `waiting_mail` | Mail not answering; 60 s probe heartbeats, 24 h cap |
| `running` | crawling (or a bulk op in flight) |
| `done` | crawl job finished (`failures` reset to 0) |
| `failed` | crawl transport/identity/write failure (`failures` incremented) |
| `cancelled` | SIGTERM received after the job started (preserves `failures`) |
| `partial` | bulk move/delete that verified with remaining > 0 |

The `failures` counter contract belongs to crawl jobs: bulk terminal writes
(`done`/`partial`/`failed`) always preserve it.

Counters: `processed` counts rows scanned (unreadable rows included) for
crawls, or the estimated/remaining selection count for bulk ops; `found`
counts records staged (always 0 for bulk jobs). `window` is a free-form,
transient label: the raw `<fromMs>-<toMs>` epoch-ms range while walking
(plus ` skipped=N` when rows were skipped inside that mailbox-window —
unrelated to the `skipped` mailbox counter), `attempt n/1440 — <error>`
while waiting for Mail, or a skip/mirror-failure reason (`skipped: zero
yield…`, `skipped: order undetectable…`). Each mailbox overwrites it, and
the terminal `done` write clears it.

A pre-start SIGTERM writes no state at all — a never-crawled install that is
cancelled immediately stays `never_run` (and so stays eagerly
respawn-eligible).

### Batch files

`<data-dir>/batches/<jobID>-<seq>.jsonl` (job IDs look like
`crawl-<unix time>`; the Calendar/Reminders/Contacts mirrors continue the
same job's numbering). NDJSON, one `Record` per line matching
`bite_index::store::Record` (`app`/`id` required; `content` omitted entirely
when the store-body posture is off / `--no-body`), written 0600 via unique
tmp + atomic rename. Mail batches hold at most 50 records; the
Calendar/Reminders/Contacts mirrors stage up to 500. Stale `*.tmp` files are
swept at job start (10 min age). Ingest deletes batches on success
(merge-insert makes re-ingest idempotent).

Coverage: the newest 30 days first, then 10-day backfill batches to a
365-day horizon (~34 windows per job); each mailbox×window walk covers at
most 20,000 messages and also stops at its window's date edge.
`window_days` is currently accepted (tool param and `--window-days`) but
ignored by the worker, and store-body-off (`--no-body`) is currently
reachable only via the undeclared `no_body` MCP param on `index_rebuild` —
the CLI has no flag.

Unparseable batches are quarantined to `batches/quarantine/` — ingest
continues; the quarantine keeps at most 200 files (evicted oldest-by-mtime).
Quarantined files are counted as `quarantined` in the ingest output and
toward `pending_batches`.

### Worker lifecycle

`crawl.pid` holds the pid of the detached worker — shared by crawl and bulk
jobs, so `index_crawl_cancel` cancels a running bulk op too. It sends
SIGTERM, polls up to 3 s for exit, then honestly reports a survivor. The
worker never deletes its own pidfile; stale entries are removed lazily by
the control plane's identity-checked liveness probe (a pid is trusted only
while `ps` confirms it is a bite-crawl process) — don't `kill $(cat
crawl.pid)` blindly.

Auto-refresh respawn (15-min cooldown per process) is eager only for
`never_run` and `failed` (the latter only while `failures < 5`). Everything
else — `done`, `cancelled`, `partial` (a bulk op's terminal state, with no
live process afterwards), `waiting_mail`, and a crashed worker's residual
`running` — comes back only via staleness (default 24 h). Any bulk op's
terminal write refreshes `updated_at`, resetting that clock.

`bite index status` reports `pending_batches` (staged + quarantined batch
files), `crawl.state` (the crawl-state.json state) and `crawl.alive` (a live
bite-crawl process owns the pidfile). `waiting_mail` or `running` with
`alive: false` means the worker is gone — run `bite index rebuild` to
respawn immediately.

Skipped mailboxes are not persisted per-mailbox: the reason lives transiently
in `window` and `skipped` is a cumulative count, so zero-yield mailboxes are
retried every job (a skip is not a job failure — the walk moves on). To
re-index one deliberately, run a targeted rebuild with a mailbox filter
(`bite index rebuild --mailbox <name>`).
