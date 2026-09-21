//! Registry of every tool: one static table drives the MCP `tools/list`
//! schemas, `tools/call` dispatch, and the CLI verbs.

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    Str,
    Int,
    Bool,
    Date,
    StrArray,
    Obj,
}

#[derive(Debug, Clone)]
pub struct ParamSpec {
    pub name: &'static str,
    pub kind: ParamKind,
    pub required: bool,
    pub desc: &'static str,
}

impl ParamSpec {
    const fn new(name: &'static str, kind: ParamKind, required: bool, desc: &'static str) -> Self {
        Self {
            name,
            kind,
            required,
            desc,
        }
    }
    pub const fn req(name: &'static str, kind: ParamKind, desc: &'static str) -> Self {
        Self::new(name, kind, true, desc)
    }
    pub const fn opt(name: &'static str, kind: ParamKind, desc: &'static str) -> Self {
        Self::new(name, kind, false, desc)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum App {
    Calendar,
    Reminders,
    Mail,
    Notes,
    Contacts,
    Messages,
}

impl App {
    pub fn as_str(&self) -> &'static str {
        match self {
            App::Calendar => "calendar",
            App::Reminders => "reminders",
            App::Mail => "mail",
            App::Notes => "notes",
            App::Contacts => "contacts",
            App::Messages => "messages",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tool {
    /// MCP tool name / canonical id, e.g. `calendar_create_event`.
    pub name: &'static str,
    pub app: App,
    /// Bridge method, e.g. `calendar.event_create`.
    pub method: &'static str,
    pub description: &'static str,
    pub params: &'static [ParamSpec],
    /// Destructive tools require `confirm: true` (the helper returns a preview
    /// otherwise — the agent shows it to the user first).
    pub destructive: bool,
}

const fn t(
    name: &'static str,
    app: App,
    method: &'static str,
    description: &'static str,
    params: &'static [ParamSpec],
) -> Tool {
    Tool {
        name,
        app,
        method,
        description,
        params,
        destructive: false,
    }
}

/// Destructive tool: helper returns a preview until `confirm: true`.
const fn td(
    name: &'static str,
    app: App,
    method: &'static str,
    description: &'static str,
    params: &'static [ParamSpec],
) -> Tool {
    Tool {
        name,
        app,
        method,
        description,
        params,
        destructive: true,
    }
}

macro_rules! ps {
    ($($spec:expr),* $(,)?) => { &[$($spec),*] };
}

pub static TOOLS: &[Tool] = &[
    // ─── Calendar ───────────────────────────────────────────────────────────
    t("calendar_list_calendars", App::Calendar, "calendar.list_calendars",
        "List the user's calendars (id, title, source, writable). Use ids for calendar-scoped queries.", &[]),
    t("calendar_events_search", App::Calendar, "calendar.events_search",
        "Search calendar events in a time window (default: past 7 days to +14 days), optionally filtered by calendars or text.",
        ps![
            ParamSpec::opt("from", ParamKind::Date, "window start, ISO 8601"),
            ParamSpec::opt("to", ParamKind::Date, "window end, ISO 8601"),
            ParamSpec::opt("calendar_ids", ParamKind::StrArray, "restrict to these calendar ids"),
            ParamSpec::opt("text", ParamKind::Str, "match title/notes/location substring"),
            ParamSpec::opt("limit", ParamKind::Int, "max events returned (default 50)"),
        ]),
    t("calendar_event_get", App::Calendar, "calendar.event_get",
        "Fetch one event by id.",
        ps![ParamSpec::req("id", ParamKind::Str, "event id from a search result")]),
    t("calendar_event_create", App::Calendar, "calendar.event_create",
        "Create a calendar event. Recurrence, relative and absolute alarms supported. Attendee invites are not supported yet (ROADMAP).",
        ps![
            ParamSpec::req("title", ParamKind::Str, "event title"),
            ParamSpec::req("start", ParamKind::Date, "start, ISO 8601 with offset"),
            ParamSpec::opt("end", ParamKind::Date, "end; default start+1h"),
            ParamSpec::opt("duration_minutes", ParamKind::Int, "alternative to end"),
            ParamSpec::opt("all_day", ParamKind::Bool, "all-day event"),
            ParamSpec::opt("calendar_id", ParamKind::Str, "target calendar id; default first writable"),
            ParamSpec::opt("location", ParamKind::Str, "location"),
            ParamSpec::opt("notes", ParamKind::Str, "notes (markdown ok)"),
            ParamSpec::opt("url", ParamKind::Str, "url"),
            ParamSpec::opt("alarms_minutes", ParamKind::StrArray, "minutes before start for alerts, e.g. [\"10\",\"60\"]"),
            ParamSpec::opt("alarm_times", ParamKind::StrArray, "absolute alarm datetimes, ISO 8601"),
            ParamSpec::opt("recurrence", ParamKind::Obj, r#"{"freq":"daily|weekly|monthly|yearly","interval":N,"count":N or "until":"ISO"}"#),
        ]),
    t("calendar_event_update", App::Calendar, "calendar.event_update",
        "Update event fields (reschedule by passing start/end, move calendar by calendar_id). Single-occurrence edit for recurring events.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "event id"),
            ParamSpec::opt("title", ParamKind::Str, "new title"),
            ParamSpec::opt("start", ParamKind::Date, "new start"),
            ParamSpec::opt("end", ParamKind::Date, "new end"),
            ParamSpec::opt("all_day", ParamKind::Bool, "all-day flag"),
            ParamSpec::opt("location", ParamKind::Str, "location"),
            ParamSpec::opt("notes", ParamKind::Str, "notes"),
            ParamSpec::opt("url", ParamKind::Str, "url"),
            ParamSpec::opt("calendar_id", ParamKind::Str, "move to this calendar"),
            ParamSpec::opt("alarms_minutes", ParamKind::StrArray, "replace relative alarms"),
            ParamSpec::opt("alarm_times", ParamKind::StrArray, "replace absolute alarms"),
            ParamSpec::opt("recurrence", ParamKind::Obj, "replace recurrence rule"),
        ]),
    td("calendar_event_delete", App::Calendar, "calendar.event_delete",
        "Delete an event. First call without confirm returns a preview; pass confirm:true to actually delete.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "event id"),
            ParamSpec::opt("confirm", ParamKind::Bool, "true to perform the deletion"),
        ]),
    t("calendar_availability", App::Calendar, "calendar.availability",
        "Free/busy between two times: busy intervals (merged) and free gaps. Use before scheduling.",
        ps![
            ParamSpec::req("from", ParamKind::Date, "window start"),
            ParamSpec::req("to", ParamKind::Date, "window end"),
            ParamSpec::opt("calendar_ids", ParamKind::StrArray, "restrict to these calendar ids"),
        ]),

    // ─── Reminders ──────────────────────────────────────────────────────────
    t("reminders_list_lists", App::Reminders, "reminders.list_lists",
        "List reminder lists.", &[]),
    t("reminders_search", App::Reminders, "reminders.search",
        "Search reminders; filter by list, text, completion state, or due within N days (sorted by due date).",
        ps![
            ParamSpec::opt("list", ParamKind::Str, "list name"),
            ParamSpec::opt("list_id", ParamKind::Str, "list id"),
            ParamSpec::opt("text", ParamKind::Str, "match title/notes substring"),
            ParamSpec::opt("completed", ParamKind::Bool, "completed state; omit for all"),
            ParamSpec::opt("due_within_days", ParamKind::Int, "only incomplete due within N days"),
            ParamSpec::opt("limit", ParamKind::Int, "max results (default 100)"),
        ]),
    t("reminders_create", App::Reminders, "reminders.create",
        "Create a reminder with optional due date and absolute time alarm.",
        ps![
            ParamSpec::req("title", ParamKind::Str, "reminder title"),
            ParamSpec::opt("list", ParamKind::Str, "list name; default first list"),
            ParamSpec::opt("list_id", ParamKind::Str, "list id"),
            ParamSpec::opt("notes", ParamKind::Str, "notes"),
            ParamSpec::opt("url", ParamKind::Str, "url"),
            ParamSpec::opt("priority", ParamKind::Int, "1 (high) to 9 (low)"),
            ParamSpec::opt("due", ParamKind::Date, "due datetime or date (due_has_time=false)"),
            ParamSpec::opt("due_has_time", ParamKind::Bool, "default true"),
            ParamSpec::opt("alarm_time", ParamKind::Date, "absolute alarm"),
        ]),
    t("reminders_update", App::Reminders, "reminders.update",
        "Update a reminder: rename, reschedule, complete/uncomplete, move list, set alarm.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "reminder id"),
            ParamSpec::opt("title", ParamKind::Str, "new title"),
            ParamSpec::opt("notes", ParamKind::Str, "notes"),
            ParamSpec::opt("url", ParamKind::Str, "url"),
            ParamSpec::opt("priority", ParamKind::Int, "1-9"),
            ParamSpec::opt("completed", ParamKind::Bool, "mark complete/incomplete"),
            ParamSpec::opt("due", ParamKind::Date, "due; pass explicit null-ish removal not supported, set new value"),
            ParamSpec::opt("due_has_time", ParamKind::Bool, "default true"),
            ParamSpec::opt("alarm_time", ParamKind::Date, "replace alarm"),
            ParamSpec::opt("list", ParamKind::Str, "move to list by name"),
            ParamSpec::opt("list_id", ParamKind::Str, "move to list by id"),
        ]),
    td("reminders_delete", App::Reminders, "reminders.delete",
        "Delete a reminder. First call without confirm returns a preview.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "reminder id"),
            ParamSpec::opt("confirm", ParamKind::Bool, "true to perform the deletion"),
        ]),

    // ─── Mail ───────────────────────────────────────────────────────────────
    t("mail_accounts", App::Mail, "mail.accounts", "List configured mail accounts.", &[]),
    t("mail_mailboxes_list", App::Mail, "mail.mailboxes",
        "List mailboxes (with unread counts).",
        ps![ParamSpec::opt("account", ParamKind::Str, "account name filter")]),
    t("mail_messages_search", App::Mail, "mail.messages_search",
        "Search messages in a mailbox, newest first (scan-capped; `truncated` flag is honest). Message bodies are not included — use mail_get_message.",
        ps![
            ParamSpec::opt("mailbox", ParamKind::Str, "mailbox name, default INBOX"),
            ParamSpec::opt("account", ParamKind::Str, "account name filter"),
            ParamSpec::opt("from", ParamKind::Str, "sender substring"),
            ParamSpec::opt("to", ParamKind::Str, "recipient substring"),
            ParamSpec::opt("subject", ParamKind::Str, "subject substring"),
            ParamSpec::opt("body", ParamKind::Str, "body substring (slower)"),
            ParamSpec::opt("unread", ParamKind::Bool, "read-state filter"),
            ParamSpec::opt("flagged", ParamKind::Bool, "flag filter"),
            ParamSpec::opt("since", ParamKind::Date, "sent at/after"),
            ParamSpec::opt("until", ParamKind::Date, "sent before"),
            ParamSpec::opt("limit", ParamKind::Int, "max results (default 20)"),
            ParamSpec::opt("max_scan", ParamKind::Int, "max messages examined (default 400)"),
        ]),
    t("mail_message_get", App::Mail, "mail.message_get",
        "Full message: headers, body, attachment manifest. Pass the `mailbox` from the search result for a fast lookup.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "message id"),
            ParamSpec::opt("mailbox", ParamKind::Str, "mailbox from the search result"),
            ParamSpec::opt("account", ParamKind::Str, "account name"),
        ]),
    t("mail_send", App::Mail, "mail.send",
        "Send an email. Body accepts Markdown by default (html:true to pass raw HTML). Attachments are file paths.",
        ps![
            ParamSpec::req("to", ParamKind::StrArray, "recipient addresses"),
            ParamSpec::opt("cc", ParamKind::StrArray, "cc addresses"),
            ParamSpec::opt("bcc", ParamKind::StrArray, "bcc addresses"),
            ParamSpec::req("subject", ParamKind::Str, "subject"),
            ParamSpec::opt("body", ParamKind::Str, "body (markdown by default)"),
            ParamSpec::opt("html", ParamKind::Bool, "body is raw HTML"),
            ParamSpec::opt("account", ParamKind::Str, "sender account"),
            ParamSpec::opt("attachments", ParamKind::StrArray, "absolute file paths to attach"),
        ]),
    t("mail_reply", App::Mail, "mail.reply",
        "Reply to a message with a quoted original (reply_all includes cc's).",
        ps![
            ParamSpec::req("id", ParamKind::Str, "message id"),
            ParamSpec::opt("mailbox", ParamKind::Str, "mailbox from the search result"),
            ParamSpec::opt("account", ParamKind::Str, "account name"),
            ParamSpec::opt("reply_all", ParamKind::Bool, "also cc the other recipients"),
            ParamSpec::opt("body", ParamKind::Str, "reply text (markdown)"),
            ParamSpec::opt("html", ParamKind::Bool, "body is raw HTML"),
        ]),
    t("mail_forward", App::Mail, "mail.forward",
        "Forward a message with a quoted original.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "message id"),
            ParamSpec::req("to", ParamKind::StrArray, "forward to"),
            ParamSpec::opt("mailbox", ParamKind::Str, "mailbox from the search result"),
            ParamSpec::opt("account", ParamKind::Str, "account name"),
            ParamSpec::opt("body", ParamKind::Str, "cover text (markdown)"),
            ParamSpec::opt("html", ParamKind::Bool, "body is raw HTML"),
        ]),
    td("mail_move", App::Mail, "mail.move",
        "Move a message to another mailbox. First call without confirm returns what would happen.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "message id"),
            ParamSpec::req("to_mailbox", ParamKind::Str, "target mailbox name"),
            ParamSpec::opt("mailbox", ParamKind::Str, "source mailbox from the search result"),
            ParamSpec::opt("account", ParamKind::Str, "account name"),
            ParamSpec::opt("confirm", ParamKind::Bool, "true to perform the move"),
        ]),
    t("mail_mark", App::Mail, "mail.mark",
        "Mark read/unread, flag/unflag, junk/not-junk.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "message id"),
            ParamSpec::opt("mailbox", ParamKind::Str, "mailbox from the search result"),
            ParamSpec::opt("account", ParamKind::Str, "account name"),
            ParamSpec::opt("read", ParamKind::Bool, "read state"),
            ParamSpec::opt("flagged", ParamKind::Bool, "flagged state"),
            ParamSpec::opt("junk", ParamKind::Bool, "junk state"),
        ]),
    td("mail_delete", App::Mail, "mail.delete",
        "Delete a message. First call without confirm returns a preview.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "message id"),
            ParamSpec::opt("mailbox", ParamKind::Str, "mailbox from the search result"),
            ParamSpec::opt("account", ParamKind::Str, "account name"),
            ParamSpec::opt("confirm", ParamKind::Bool, "true to perform the deletion"),
        ]),
    t("mail_attachment_save", App::Mail, "mail.attachment_save",
        "Save a message attachment to disk (by index from mail_get_message).",
        ps![
            ParamSpec::req("id", ParamKind::Str, "message id"),
            ParamSpec::opt("mailbox", ParamKind::Str, "mailbox from the search result"),
            ParamSpec::opt("account", ParamKind::Str, "account name"),
            ParamSpec::opt("index", ParamKind::Int, "attachment index (default 0)"),
            ParamSpec::opt("dir", ParamKind::Str, "target directory (default temp)"),
        ]),

    // ─── Notes ──────────────────────────────────────────────────────────────
    t("notes_folders", App::Notes, "notes.folders", "List Notes folders per account.",
        ps![ParamSpec::opt("account", ParamKind::Str, "account name filter")]),
    t("notes_search", App::Notes, "notes.search",
        "Search notes by text (title or body), optionally in one folder.",
        ps![
            ParamSpec::opt("text", ParamKind::Str, "substring match"),
            ParamSpec::opt("folder", ParamKind::Str, "folder name"),
            ParamSpec::opt("account", ParamKind::Str, "account name"),
            ParamSpec::opt("limit", ParamKind::Int, "max results (default 25)"),
        ]),
    t("notes_get", App::Notes, "notes.get",
        "Full note: HTML body plus a markdown rendering.",
        ps![ParamSpec::req("id", ParamKind::Str, "note id")]),
    t("notes_create", App::Notes, "notes.create",
        "Create a note. `body` accepts Markdown (converted to Notes HTML).",
        ps![
            ParamSpec::opt("title", ParamKind::Str, "note title; default 'New Note'"),
            ParamSpec::opt("body", ParamKind::Str, "markdown body"),
            ParamSpec::opt("folder", ParamKind::Str, "folder name; default first"),
            ParamSpec::opt("account", ParamKind::Str, "account name"),
            ParamSpec::opt("html", ParamKind::Bool, "body is raw HTML"),
        ]),
    t("notes_update", App::Notes, "notes.update",
        "Update a note: set/append body (markdown), rename, or move folder.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "note id"),
            ParamSpec::opt("title", ParamKind::Str, "new title"),
            ParamSpec::opt("body", ParamKind::Str, "markdown body (replaces unless append)"),
            ParamSpec::opt("append", ParamKind::Bool, "append instead of replace"),
            ParamSpec::opt("folder", ParamKind::Str, "move to this folder"),
            ParamSpec::opt("html", ParamKind::Bool, "body is raw HTML"),
        ]),
    td("notes_delete", App::Notes, "notes.delete",
        "Delete a note. First call without confirm returns a preview.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "note id"),
            ParamSpec::opt("confirm", ParamKind::Bool, "true to perform the deletion"),
        ]),

    // ─── Contacts ───────────────────────────────────────────────────────────
    t("contacts_search", App::Contacts, "contacts.search",
        "Search contacts by name/email/phone substring.",
        ps![
            ParamSpec::req("query", ParamKind::Str, "substring to match"),
            ParamSpec::opt("limit", ParamKind::Int, "max results (default 25)"),
        ]),
    t("contacts_get", App::Contacts, "contacts.get", "Fetch one contact by id.",
        ps![ParamSpec::req("id", ParamKind::Str, "contact id")]),
    t("contacts_create", App::Contacts, "contacts.create",
        "Create a contact. emails/phones/urls are arrays of {label,value}.",
        ps![
            ParamSpec::opt("first", ParamKind::Str, "given name"),
            ParamSpec::opt("last", ParamKind::Str, "family name"),
            ParamSpec::opt("org", ParamKind::Str, "organization"),
            ParamSpec::opt("note", ParamKind::Str, "note"),
            ParamSpec::opt("emails", ParamKind::StrArray, r#"[{"label":"work","value":"a@b.c"}]"#),
            ParamSpec::opt("phones", ParamKind::StrArray, r#"[{"label":"mobile","value":"+1555…"}]"#),
            ParamSpec::opt("urls", ParamKind::StrArray, r#"[{"label":"home","value":"https://…"}]"#),
        ]),
    t("contacts_update", App::Contacts, "contacts.update",
        "Update contact fields. emails/phones/urls replace the whole list when passed.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "contact id"),
            ParamSpec::opt("first", ParamKind::Str, "given name"),
            ParamSpec::opt("last", ParamKind::Str, "family name"),
            ParamSpec::opt("org", ParamKind::Str, "organization"),
            ParamSpec::opt("note", ParamKind::Str, "note"),
            ParamSpec::opt("emails", ParamKind::StrArray, "replace all emails"),
            ParamSpec::opt("phones", ParamKind::StrArray, "replace all phones"),
            ParamSpec::opt("urls", ParamKind::StrArray, "replace all urls"),
        ]),
    td("contacts_delete", App::Contacts, "contacts.delete",
        "Delete a contact. First call without confirm returns a preview.",
        ps![
            ParamSpec::req("id", ParamKind::Str, "contact id"),
            ParamSpec::opt("confirm", ParamKind::Bool, "true to perform the deletion"),
        ]),
    t("contacts_groups", App::Contacts, "contacts.groups", "List contact groups with member counts.", &[]),

    // ─── Messages ───────────────────────────────────────────────────────────
    t("messages_send", App::Messages, "messages.send",
        "Send an iMessage/SMS. The recipient must be reachable in Messages (have a conversation history) — routing is automatic.",
        ps![
            ParamSpec::req("to", ParamKind::Str, "phone or email of the recipient"),
            ParamSpec::req("text", ParamKind::Str, "message text"),
        ]),
    t("messages_chats_recent", App::Messages, "messages.chats_recent",
        "List recent chats with participants and last message.",
        ps![ParamSpec::opt("limit", ParamKind::Int, "max chats (default 20)")]),
    t("messages_history", App::Messages, "messages.history",
        "Read a chat's recent history (newest last). Active chats come from Messages; older history needs Full Disk Access and falls back to chat.db.",
        ps![
            ParamSpec::req("chat_id", ParamKind::Str, "chat id from messages_chats_recent"),
            ParamSpec::opt("limit", ParamKind::Int, "max messages (default 100)"),
        ]),
];

pub fn tools() -> &'static [Tool] {
    TOOLS
}

pub fn find(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.name == name)
}

/// JSON Schema (subset) for MCP tools/list.
pub fn json_schema(tool: &Tool) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for p in tool.params {
        let schema = match p.kind {
            ParamKind::Str => json!({ "type": "string" }),
            ParamKind::Int => json!({ "type": "integer" }),
            ParamKind::Bool => json!({ "type": "boolean" }),
            ParamKind::Date => json!({ "type": "string", "description": "ISO 8601 date(-time)" }),
            ParamKind::StrArray => json!({ "type": "array", "items": { "type": "string" } }),
            ParamKind::Obj => json!({ "type": "object" }),
        };
        let mut schema = schema;
        if let Value::Object(ref mut map) = schema {
            if p.kind != ParamKind::Date {
                map.insert("description".into(), json!(p.desc));
            }
        }
        properties.insert(p.name.to_string(), schema);
        if p.required {
            required.push(json!(p.name));
        }
    }
    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), json!(required));
    }
    Value::Object(schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_count_and_uniqueness() {
        assert!(
            TOOLS.len() >= 38,
            "expected full surface, got {}",
            TOOLS.len()
        );
        let mut names: Vec<_> = TOOLS.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TOOLS.len(), "duplicate tool names");
    }

    #[test]
    fn bridge_methods_exist_in_swift_registry() {
        // Every method here must exist in the Swift dispatcher. This list is
        // checked against the Swift source by tests/protocol_conformance in the
        // workspace; here we assert all methods are namespaced correctly.
        for t in TOOLS {
            let (ns, _rest) = t.method.split_once('.').expect("method must be namespaced");
            let expected_ns = match t.app {
                App::Calendar => "calendar",
                App::Reminders => "reminders",
                App::Mail => "mail",
                App::Notes => "notes",
                App::Contacts => "contacts",
                App::Messages => "messages",
            };
            assert_eq!(ns, expected_ns, "method {} has wrong namespace", t.method);
        }
    }

    #[test]
    fn json_schema_shape() {
        let tool = find("calendar_event_create").unwrap();
        let schema = json_schema(tool);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["title"]["type"], "string");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("title")));
        assert!(required.contains(&json!("start")));
    }

    #[test]
    fn destructive_tools_have_confirm_param() {
        for t in TOOLS.iter() {
            if t.destructive {
                assert!(
                    t.params.iter().any(|p| p.name == "confirm"),
                    "destructive tool {} lacks confirm",
                    t.name
                );
            }
        }
    }
}
