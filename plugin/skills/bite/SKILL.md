---
name: bite
description: Using bite to work with the user's Apple Calendar, Reminders, Mail, Notes, Contacts, and Messages. Covers tool selection, date formatting, confirmation flow for destructive actions, and handling permission errors.
---

# bite — native Apple apps for agents

bite exposes the user's real Apple data. These rules keep interactions safe
and effective.

## Core rules

1. **Dates are ISO 8601 with offset** (`2026-09-21T15:00:00+02:00`). When the
   user says "tomorrow 3pm", resolve it against the user's local timezone and
   send a full ISO timestamp. Never send naive times without an offset.
2. **Destructive actions preview first.** `calendar_event_delete`,
   `reminders_delete`, `mail_delete`, `mail_move`, `notes_delete`,
   `contacts_delete` return a `would_*` preview when called without
   `confirm: true`. Show the preview to the user, get an OK, then re-call with
   `confirm: true`. Never pass `confirm: true` on the first call unless the
   user explicitly asked for an irreversible action.
3. **Check availability before scheduling.** Call `calendar_availability` over
   the proposed window and pick a free gap before `calendar_event_create`.
4. **Permission errors are for the user.** If a tool returns an error with a
   `fix`, relay the `fix` verbatim — it is an exact command or System Settings
   deep link. Do not retry the same call before the user fixes the permission.
5. **Mail searches need a mailbox** (default `INBOX`). Follow-up calls
   (`mail_message_get`, `mail_reply`, `mail_move`, …) should pass the
   `mailbox` from the search result — lookups become single-mailbox and fast.
6. **Notes bodies are Markdown.** Send `body` as Markdown; `notes_get`
   returns both HTML and a markdown rendering.
7. **Messages recipients must exist in Messages.** If `messages_send` returns
   `not_found`, tell the user to open Messages once with that recipient.

## Common recipes

**Schedule a meeting:** `calendar_availability` (from/to) → choose a free gap
→ `calendar_event_create` with `start`, `end`, `location`, `notes` (markdown),
optional `alarms_minutes: ["15"]` and `recurrence:
{"freq":"weekly","interval":1,"count":8}`.

**Weekly review:** `calendar_events_search` (next 7 days) + `reminders_search`
(`due_within_days: 7`, `completed: false`) → summarize day by day.

**Inbox triage:** `mail_messages_search` (`unread: true`) → summarize each in
one line → propose reply/archive/flag → act only on confirmation
(`mail_mark` for read state, `mail_move` + `confirm: true` to archive).

**Contact lookup → message:** `contacts_search` → get email/phone →
`mail_send` or `messages_send`.

## Tool map

| App | Tools |
|-----|-------|
| Calendar | calendar_list_calendars, calendar_events_search, calendar_event_get, calendar_event_create, calendar_event_update, calendar_event_delete, calendar_availability |
| Reminders | reminders_list_lists, reminders_search, reminders_create, reminders_update, reminders_delete |
| Mail | mail_accounts, mail_mailboxes_list, mail_messages_search, mail_message_get, mail_send, mail_reply, mail_forward, mail_move, mail_mark, mail_delete, mail_attachment_save |
| Notes | notes_folders, notes_search, notes_get, notes_create, notes_update, notes_delete |
| Contacts | contacts_search, contacts_get, contacts_create, contacts_update, contacts_delete, contacts_groups |
| Messages | messages_send, messages_chats_recent, messages_history |

## CLI equivalent

Every tool is also a CLI verb (`bite reminders search --due-within-days 7`).
Use the CLI when the user wants to run something themselves; use MCP tools
when working autonomously. `bite doctor` diagnoses permissions.
