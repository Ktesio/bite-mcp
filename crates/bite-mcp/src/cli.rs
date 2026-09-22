//! clap CLI: every bridge tool has a mirrored verb; params funnel into
//! `bite_core::ops` exactly like MCP `tools/call` does.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "bite",
    version,
    about = "Native Apple apps (Calendar, Reminders, Mail, Notes, Contacts, Messages) for humans and AI agents",
    long_about = "bite: a Rust control plane over a native Swift helper (EventKit + ScriptingBridge).\nRun `bite mcp` as an MCP stdio server, or use the verbs directly."
)]
pub struct Cli {
    /// Emit compact machine-readable JSON (pretty JSON is the default output)
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Run the MCP stdio server (this is what agent CLIs launch)
    Mcp,
    /// Interactive onboarding: install helper, check permissions, configure agent CLIs
    Setup {
        /// Configure every detected client without asking
        #[arg(long)]
        all_clients: bool,
        /// Non-interactive: never prompt
        #[arg(long)]
        yes: bool,
    },
    /// Check environment, helper and per-app permissions (prompt-free by default)
    Doctor {
        /// Open System Settings panes for denied permissions
        #[arg(long)]
        fix: bool,
        /// Live-probe Mail/Notes/Messages Apple Events (triggers TCC prompts)
        #[arg(long)]
        probe: bool,
    },
    /// Install/refresh the native helper at its stable path (TCC-friendly)
    InstallHelper {
        #[arg(long)]
        force: bool,
    },
    /// Bridge diagnostics
    Helper {
        #[command(subcommand)]
        cmd: HelperCmd,
    },
    /// Read/write bite config
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },

    // ── Calendar ──
    Calendar {
        #[command(subcommand)]
        cmd: CalendarCmd,
    },
    // ── Reminders ──
    Reminders {
        #[command(subcommand)]
        cmd: RemindersCmd,
    },
    // ── Mail ──
    Mail {
        #[command(subcommand)]
        cmd: MailCmd,
    },
    // ── Notes ──
    Notes {
        #[command(subcommand)]
        cmd: NotesCmd,
    },
    // ── Contacts ──
    Contacts {
        #[command(subcommand)]
        cmd: ContactsCmd,
    },
    // ── Messages ──
    Messages {
        #[command(subcommand)]
        cmd: MessagesCmd,
    },
    // ── Entity index ──
    Index {
        #[command(subcommand)]
        cmd: IndexCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum IndexCmd {
    /// Rebuild/refresh the entity index (30-day window + background backfill)
    Rebuild {
        #[arg(long)]
        window_days: Option<i64>,
        #[arg(long)]
        mailbox: Option<String>,
    },
    /// Index state: rows, freshness, pending crawl batches
    Status,
    /// Cancel a running crawl
    Cancel,
    /// Destroy the entity index (requires --yes)
    Wipe {
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum HelperCmd {
    /// Ping the helper (spawns it if needed)
    Ping,
    /// Helper + protocol version
    Version,
    /// Raw bridge call: bite helper raw <method> '<json-params>'
    Raw {
        method: String,
        /// JSON object of params
        #[arg(default_value = "{}")]
        params: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Print the config file path
    Path,
    /// Print the current config
    Get,
    /// Set config values, e.g. bite config set default-calendar Home
    Set { key: String, value: String },
}

#[derive(Subcommand, Debug)]
pub enum CalendarCmd {
    /// List calendars
    ListCalendars,
    /// Search events; dates accept ISO or "tomorrow 3pm" style
    SearchEvents {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        calendar_ids: Vec<String>,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Get one event
    GetEvent { id: String },
    /// Create an event
    CreateEvent {
        #[arg(long)]
        title: String,
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: Option<String>,
        #[arg(long)]
        duration_minutes: Option<i64>,
        #[arg(long)]
        all_day: bool,
        #[arg(long)]
        calendar_id: Option<String>,
        #[arg(long)]
        location: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        alarms_minutes: Vec<String>,
    },
    /// Update an event
    UpdateEvent {
        id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        start: Option<String>,
        #[arg(long)]
        end: Option<String>,
        #[arg(long)]
        location: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        calendar_id: Option<String>,
    },
    /// Delete an event (asks for --yes)
    DeleteEvent {
        id: String,
        #[arg(long)]
        yes: bool,
    },
    /// Free/busy for a window
    Availability {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum RemindersCmd {
    /// List reminder lists
    Lists,
    /// Search reminders
    Search {
        #[arg(long)]
        list: Option<String>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        completed: Option<bool>,
        #[arg(long)]
        due_within_days: Option<i64>,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Create a reminder
    Create {
        title: String,
        #[arg(long)]
        list: Option<String>,
        #[arg(long)]
        due: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        alarm_time: Option<String>,
    },
    /// Update a reminder
    Update {
        id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        due: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        completed: Option<bool>,
        #[arg(long)]
        list: Option<String>,
    },
    /// Delete a reminder (asks for --yes)
    Delete {
        id: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum MailCmd {
    /// List mail accounts
    Accounts,
    /// List mailboxes
    Mailboxes {
        #[arg(long)]
        account: Option<String>,
    },
    /// Search messages (newest first)
    Search {
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        unread: Option<bool>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        flagged: Option<bool>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
        #[arg(long)]
        max_scan: Option<i64>,
    },
    /// Get a full message
    Get {
        id: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long)]
        account: Option<String>,
    },
    /// Send an email (markdown body by default)
    Send {
        #[arg(long)]
        to: Vec<String>,
        #[arg(long)]
        cc: Vec<String>,
        #[arg(long)]
        bcc: Vec<String>,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        html: bool,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        attachments: Vec<String>,
    },
    /// Reply to a message
    Reply {
        id: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        reply_all: bool,
        #[arg(long)]
        body: Option<String>,
    },
    /// Forward a message
    Forward {
        id: String,
        #[arg(long)]
        to: Vec<String>,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long)]
        body: Option<String>,
    },
    /// Move a message (asks for --yes)
    Move {
        id: String,
        #[arg(long)]
        to_mailbox: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Mark read/flagged/junk
    Mark {
        id: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        read: Option<bool>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        flagged: Option<bool>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        junk: Option<bool>,
    },
    /// Delete a message (asks for --yes)
    Delete {
        id: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Save an attachment
    SaveAttachment {
        id: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long)]
        index: Option<i64>,
        #[arg(long)]
        dir: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum NotesCmd {
    /// List folders
    Folders,
    /// Search notes
    Search {
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Get a note (HTML + markdown)
    Get { id: String },
    /// Create a note (markdown body)
    Create {
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        folder: Option<String>,
    },
    /// Update a note
    Update {
        id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        append: bool,
        #[arg(long)]
        folder: Option<String>,
    },
    /// Delete a note (asks for --yes)
    Delete {
        id: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ContactsCmd {
    /// Search contacts
    Search {
        query: String,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Get a contact
    Get { id: String },
    /// Create a contact
    Create {
        #[arg(long)]
        first: Option<String>,
        #[arg(long)]
        last: Option<String>,
        #[arg(long)]
        org: Option<String>,
        #[arg(long)]
        note: Option<String>,
        /// repeatable: --email work=a@b.c
        #[arg(long = "email")]
        emails: Vec<String>,
        /// repeatable: --phone mobile=+1555…
        #[arg(long = "phone")]
        phones: Vec<String>,
    },
    /// Update a contact
    Update {
        id: String,
        #[arg(long)]
        first: Option<String>,
        #[arg(long)]
        last: Option<String>,
        #[arg(long)]
        org: Option<String>,
    },
    /// Delete a contact (asks for --yes)
    Delete {
        id: String,
        #[arg(long)]
        yes: bool,
    },
    /// List groups
    Groups,
}

#[derive(Subcommand, Debug)]
pub enum MessagesCmd {
    /// Send a message (recipient must exist in Messages)
    Send { to: String, text: String },
    /// Recent chats
    Chats {
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Chat history
    History {
        chat_id: String,
        #[arg(long)]
        limit: Option<i64>,
    },
}
