# bite-mcp

`bite` — native Apple apps (Calendar, Reminders, Mail, Notes, Contacts,
Messages) for AI agents, over MCP. This crate installs the `bite` binary:
an MCP stdio server plus a CLI, with a native Swift helper it compiles and
installs locally (EventKit + ScriptingBridge — no `osascript`).

**Install:** `cargo install bite-mcp` (needs Xcode Command Line Tools to
build the Swift helper), then run `bite setup`.

Full documentation, the marketplace plugin, and the source live at
<https://github.com/ktesio/bite-mcp>.
