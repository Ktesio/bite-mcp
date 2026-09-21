# Bridge Protocol (Rust ↔ Swift helper)

Line-delimited JSON (NDJSON) over the helper's stdin/stdout. One JSON value
per line, UTF-8, `\n` terminated. Helper stderr is free-form diagnostics
(mirrored by the Rust side to its own stderr).

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
