# Privacy Statement

bite is a **local-only** tool. This statement describes exactly what data it
can touch and where that data goes. (Short answer: nowhere.)

## What bite can access

When you grant the corresponding macOS permissions, bite can read and modify,
on the machine you run it on:

- **Calendar** — events in your calendars (EventKit)
- **Reminders** — your reminder lists and items (EventKit)
- **Mail** — messages, mailboxes, and attachments of accounts configured in
  Apple Mail (Apple Events / ScriptingBridge)
- **Notes** — your notes in Apple Notes (Apple Events / ScriptingBridge)
- **Contacts** — your address book (Contacts framework)
- **Messages** — sending iMessage/SMS via Apple Messages, recent chats, and —
  only if you grant Full Disk Access — your chat history database
  (`~/Library/Messages/chat.db`)

## Where the data goes

**Nowhere.** bite has no server, no account system, no analytics, no
telemetry, and no crash reporting. All processing happens in two local
processes on your machine (`bite` and its Swift helper). The only network
activity in the entire project is the optional, explicit `bite install-helper
--prebuilt` download (roadmap), which fetches a signed helper binary from the
GitHub Releases page and involves none of your data.

One caveat you control: bite is an **agent tool**. Whatever AI agent you
connect it to (Claude, Codex, ZCode, OpenCode, …) can read the tool results —
so the agent's own privacy policy and data handling apply to anything it does
with your data. bite itself adds no network hop; choose agents you trust.

## Your control

- Every permission is granted through macOS's own TCC prompts and can be
  reviewed or revoked at any time in **System Settings → Privacy & Security**
  (and Full Disk Access). Revoking takes effect immediately.
- `bite doctor` shows, without triggering any prompt, which permissions are
  granted, pending, or denied.
- Destructive operations (deleting events/reminders/mail/notes/contacts,
  moving mail) always return a preview first and require an explicit
  `confirm: true`.

## Intended use

bite is designed to operate on the data of the person running it, on their
own machine, with their knowledge. It is not designed for monitoring another
person, a device you do not own, or anyone without their consent — such use
is unlawful in most jurisdictions and is outside the purpose of this project.

## Contact

Questions or privacy concerns: open an issue on the
[GitHub repository](https://github.com/ktesio/bite-mcp).
