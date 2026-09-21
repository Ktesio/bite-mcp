//! Command dispatch: CLI verbs → registry params → ops → output.

use std::io::Write;

use bite_core::error::cli_error;
use bite_core::{ops, BiteError};
use serde_json::{json, Map, Value};

use crate::cli::{
    CalendarCmd, Cmd, ConfigCmd, ContactsCmd, MailCmd, MessagesCmd, NotesCmd, RemindersCmd,
};
use crate::helper::BridgeHandle;
use crate::{doctor, mcp, setup};

pub fn run(cli: crate::cli::Cli) -> i32 {
    match dispatch(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {}", e.render());
            1
        }
    }
}

fn dispatch(cli: crate::cli::Cli) -> Result<i32, BiteError> {
    let json_out = cli.json;
    let finish = |v: Value| -> Result<i32, BiteError> {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let text = if json_out {
            v.to_string()
        } else {
            serde_json::to_string_pretty(&v).unwrap_or_default()
        };
        writeln!(lock, "{text}").map_err(|e| cli_error(e.to_string()))?;
        Ok(0)
    };

    match cli.cmd {
        Cmd::Mcp => Ok(mcp::serve()),
        Cmd::Setup { all_clients, yes } => setup::run(all_clients, yes),
        Cmd::Doctor { fix, probe } => doctor::run(fix, probe),
        Cmd::InstallHelper { force } => {
            let path = crate::helper::install_helper(force)?;
            println!("helper installed at {}", path.display());
            Ok(0)
        }

        Cmd::Helper { cmd } => {
            let mut h = BridgeHandle::new();
            match cmd {
                crate::cli::HelperCmd::Ping => {
                    let r = h.get()?.call("sys.ping", &json!({})).map_err(BiteError)?;
                    finish(r)
                }
                crate::cli::HelperCmd::Version => {
                    let r = h
                        .get()?
                        .call("sys.version", &json!({}))
                        .map_err(BiteError)?;
                    finish(r)
                }
                crate::cli::HelperCmd::Raw { method, params } => {
                    let params: Value = serde_json::from_str(&params)
                        .map_err(|e| cli_error(format!("params must be JSON: {e}")))?;
                    let r = h.get()?.call(&method, &params).map_err(BiteError)?;
                    finish(r)
                }
            }
        }

        Cmd::Config { cmd } => match cmd {
            ConfigCmd::Path => {
                println!("{}", bite_core::config::config_path().display());
                Ok(0)
            }
            ConfigCmd::Get => finish(
                serde_json::to_value(bite_core::config::Config::load()).unwrap_or(Value::Null),
            ),
            ConfigCmd::Set { key, value } => {
                let mut cfg = bite_core::config::Config::load();
                match key.as_str() {
                    "default-calendar" | "default_calendar" => cfg.default_calendar = Some(value),
                    "default-mail-account" | "default_mail_account" => cfg.default_mail_account = Some(value),
                    "timeout-secs" | "timeout_secs" => {
                        cfg.timeout_secs = value
                            .parse::<u64>()
                            .map(Some)
                            .map_err(|_| cli_error("timeout-secs must be a number"))?
                    }
                    "release-repo" | "release_repo" => cfg.release_repo = Some(value),
                    _ => return Err(cli_error("unknown config key (default-calendar, default-mail-account, timeout-secs, release-repo)")),
                }
                cfg.save().map_err(|e| cli_error(e.to_string()))?;
                println!("saved");
                Ok(0)
            }
        },

        Cmd::Calendar { cmd } => {
            let mut h = BridgeHandle::new();
            match cmd {
                CalendarCmd::ListCalendars => {
                    finish(call(&mut h, "calendar_list_calendars", json!({}))?)
                }
                CalendarCmd::SearchEvents {
                    from,
                    to,
                    text,
                    calendar_ids,
                    limit,
                } => {
                    let mut p = Map::new();
                    insert_date(&mut p, "from", from)?;
                    insert_date(&mut p, "to", to)?;
                    insert_opt(&mut p, "text", text);
                    if !calendar_ids.is_empty() {
                        p.insert("calendar_ids".into(), json!(calendar_ids));
                    }
                    insert_opt(&mut p, "limit", limit);
                    finish(call(&mut h, "calendar_events_search", Value::Object(p))?)
                }
                CalendarCmd::GetEvent { id } => {
                    finish(call(&mut h, "calendar_event_get", json!({ "id": id }))?)
                }
                CalendarCmd::CreateEvent {
                    title,
                    start,
                    end,
                    duration_minutes,
                    all_day,
                    calendar_id,
                    location,
                    notes,
                    url,
                    alarms_minutes,
                } => {
                    let mut p = Map::new();
                    p.insert("title".into(), json!(title));
                    p.insert("start".into(), json!(parse_date(&start)?));
                    insert_date(&mut p, "end", end)?;
                    insert_opt(&mut p, "duration_minutes", duration_minutes);
                    p.insert("all_day".into(), json!(all_day));
                    insert_opt(&mut p, "calendar_id", calendar_id);
                    insert_opt(&mut p, "location", location);
                    insert_opt(&mut p, "notes", notes);
                    insert_opt(&mut p, "url", url);
                    if !alarms_minutes.is_empty() {
                        p.insert("alarms_minutes".into(), json!(alarms_minutes));
                    }
                    finish(call(&mut h, "calendar_event_create", Value::Object(p))?)
                }
                CalendarCmd::UpdateEvent {
                    id,
                    title,
                    start,
                    end,
                    location,
                    notes,
                    calendar_id,
                } => {
                    let mut p = Map::new();
                    p.insert("id".into(), json!(id));
                    insert_opt(&mut p, "title", title);
                    insert_date(&mut p, "start", start)?;
                    insert_date(&mut p, "end", end)?;
                    insert_opt(&mut p, "location", location);
                    insert_opt(&mut p, "notes", notes);
                    insert_opt(&mut p, "calendar_id", calendar_id);
                    finish(call(&mut h, "calendar_event_update", Value::Object(p))?)
                }
                CalendarCmd::DeleteEvent { id, yes } => finish(call(
                    &mut h,
                    "calendar_event_delete",
                    json!({ "id": id, "confirm": yes }),
                )?),
                CalendarCmd::Availability { from, to } => finish(call(
                    &mut h,
                    "calendar_availability",
                    json!({ "from": parse_date(&from)?, "to": parse_date(&to)? }),
                )?),
            }
        }

        Cmd::Reminders { cmd } => {
            let mut h = BridgeHandle::new();
            match cmd {
                RemindersCmd::Lists => finish(call(&mut h, "reminders_list_lists", json!({}))?),
                RemindersCmd::Search {
                    list,
                    text,
                    completed,
                    due_within_days,
                    limit,
                } => {
                    let mut p = Map::new();
                    insert_opt(&mut p, "list", list);
                    insert_opt(&mut p, "text", text);
                    insert_opt(&mut p, "completed", completed);
                    insert_opt(&mut p, "due_within_days", due_within_days);
                    insert_opt(&mut p, "limit", limit);
                    finish(call(&mut h, "reminders_search", Value::Object(p))?)
                }
                RemindersCmd::Create {
                    title,
                    list,
                    due,
                    notes,
                    priority,
                    alarm_time,
                } => {
                    let mut p = Map::new();
                    p.insert("title".into(), json!(title));
                    insert_opt(&mut p, "list", list);
                    insert_date(&mut p, "due", due)?;
                    insert_opt(&mut p, "notes", notes);
                    insert_opt(&mut p, "priority", priority);
                    insert_date(&mut p, "alarm_time", alarm_time)?;
                    finish(call(&mut h, "reminders_create", Value::Object(p))?)
                }
                RemindersCmd::Update {
                    id,
                    title,
                    due,
                    completed,
                    list,
                } => {
                    let mut p = Map::new();
                    p.insert("id".into(), json!(id));
                    insert_opt(&mut p, "title", title);
                    insert_date(&mut p, "due", due)?;
                    insert_opt(&mut p, "completed", completed);
                    insert_opt(&mut p, "list", list);
                    finish(call(&mut h, "reminders_update", Value::Object(p))?)
                }
                RemindersCmd::Delete { id, yes } => finish(call(
                    &mut h,
                    "reminders_delete",
                    json!({ "id": id, "confirm": yes }),
                )?),
            }
        }

        Cmd::Mail { cmd } => {
            let mut h = BridgeHandle::new();
            match cmd {
                MailCmd::Accounts => finish(call(&mut h, "mail_accounts", json!({}))?),
                MailCmd::Mailboxes { account } => {
                    let mut p = Map::new();
                    insert_opt(&mut p, "account", account);
                    finish(call(&mut h, "mail_mailboxes_list", Value::Object(p))?)
                }
                MailCmd::Search {
                    mailbox,
                    account,
                    from,
                    to,
                    subject,
                    body,
                    unread,
                    flagged,
                    since,
                    until,
                    limit,
                    max_scan,
                } => {
                    let mut p = Map::new();
                    p.insert("mailbox".into(), json!(mailbox));
                    insert_opt(&mut p, "account", account);
                    insert_opt(&mut p, "from", from);
                    insert_opt(&mut p, "to", to);
                    insert_opt(&mut p, "subject", subject);
                    insert_opt(&mut p, "body", body);
                    insert_opt(&mut p, "unread", unread);
                    insert_opt(&mut p, "flagged", flagged);
                    insert_date(&mut p, "since", since)?;
                    insert_date(&mut p, "until", until)?;
                    insert_opt(&mut p, "limit", limit);
                    insert_opt(&mut p, "max_scan", max_scan);
                    finish(call(&mut h, "mail_messages_search", Value::Object(p))?)
                }
                MailCmd::Get {
                    id,
                    mailbox,
                    account,
                } => {
                    let mut p = Map::new();
                    p.insert("id".into(), json!(id));
                    insert_opt(&mut p, "mailbox", mailbox);
                    insert_opt(&mut p, "account", account);
                    finish(call(&mut h, "mail_message_get", Value::Object(p))?)
                }
                MailCmd::Send {
                    to,
                    cc,
                    bcc,
                    subject,
                    body,
                    html,
                    account,
                    attachments,
                } => {
                    let mut p = Map::new();
                    p.insert("to".into(), json!(to));
                    if !cc.is_empty() {
                        p.insert("cc".into(), json!(cc));
                    }
                    if !bcc.is_empty() {
                        p.insert("bcc".into(), json!(bcc));
                    }
                    p.insert("subject".into(), json!(subject));
                    insert_opt(&mut p, "body", body);
                    p.insert("html".into(), json!(html));
                    insert_opt(&mut p, "account", account);
                    if !attachments.is_empty() {
                        p.insert("attachments".into(), json!(attachments));
                    }
                    finish(call(&mut h, "mail_send", Value::Object(p))?)
                }
                MailCmd::Reply {
                    id,
                    mailbox,
                    account,
                    reply_all,
                    body,
                } => {
                    let mut p = Map::new();
                    p.insert("id".into(), json!(id));
                    insert_opt(&mut p, "mailbox", mailbox);
                    insert_opt(&mut p, "account", account);
                    p.insert("reply_all".into(), json!(reply_all));
                    insert_opt(&mut p, "body", body);
                    finish(call(&mut h, "mail_reply", Value::Object(p))?)
                }
                MailCmd::Forward {
                    id,
                    to,
                    mailbox,
                    body,
                } => {
                    let mut p = Map::new();
                    p.insert("id".into(), json!(id));
                    p.insert("to".into(), json!(to));
                    insert_opt(&mut p, "mailbox", mailbox);
                    insert_opt(&mut p, "body", body);
                    finish(call(&mut h, "mail_forward", Value::Object(p))?)
                }
                MailCmd::Move {
                    id,
                    to_mailbox,
                    mailbox,
                    account,
                    yes,
                } => {
                    let mut p = Map::new();
                    p.insert("id".into(), json!(id));
                    p.insert("to_mailbox".into(), json!(to_mailbox));
                    insert_opt(&mut p, "mailbox", mailbox);
                    insert_opt(&mut p, "account", account);
                    p.insert("confirm".into(), json!(yes));
                    finish(call(&mut h, "mail_move", Value::Object(p))?)
                }
                MailCmd::Mark {
                    id,
                    mailbox,
                    read,
                    flagged,
                    junk,
                } => {
                    let mut p = Map::new();
                    p.insert("id".into(), json!(id));
                    insert_opt(&mut p, "mailbox", mailbox);
                    insert_opt(&mut p, "read", read);
                    insert_opt(&mut p, "flagged", flagged);
                    insert_opt(&mut p, "junk", junk);
                    finish(call(&mut h, "mail_mark", Value::Object(p))?)
                }
                MailCmd::Delete { id, mailbox, yes } => {
                    let mut p = Map::new();
                    p.insert("id".into(), json!(id));
                    insert_opt(&mut p, "mailbox", mailbox);
                    p.insert("confirm".into(), json!(yes));
                    finish(call(&mut h, "mail_delete", Value::Object(p))?)
                }
                MailCmd::SaveAttachment {
                    id,
                    mailbox,
                    index,
                    dir,
                } => {
                    let mut p = Map::new();
                    p.insert("id".into(), json!(id));
                    insert_opt(&mut p, "mailbox", mailbox);
                    insert_opt(&mut p, "index", index);
                    insert_opt(&mut p, "dir", dir);
                    finish(call(&mut h, "mail_attachment_save", Value::Object(p))?)
                }
            }
        }

        Cmd::Notes { cmd } => {
            let mut h = BridgeHandle::new();
            match cmd {
                NotesCmd::Folders => finish(call(&mut h, "notes_folders", json!({}))?),
                NotesCmd::Search {
                    text,
                    folder,
                    limit,
                } => {
                    let mut p = Map::new();
                    insert_opt(&mut p, "text", text);
                    insert_opt(&mut p, "folder", folder);
                    insert_opt(&mut p, "limit", limit);
                    finish(call(&mut h, "notes_search", Value::Object(p))?)
                }
                NotesCmd::Get { id } => finish(call(&mut h, "notes_get", json!({ "id": id }))?),
                NotesCmd::Create {
                    title,
                    body,
                    folder,
                } => {
                    let mut p = Map::new();
                    insert_opt(&mut p, "title", title);
                    insert_opt(&mut p, "body", body);
                    insert_opt(&mut p, "folder", folder);
                    finish(call(&mut h, "notes_create", Value::Object(p))?)
                }
                NotesCmd::Update {
                    id,
                    title,
                    body,
                    append,
                    folder,
                } => {
                    let mut p = Map::new();
                    p.insert("id".into(), json!(id));
                    insert_opt(&mut p, "title", title);
                    insert_opt(&mut p, "body", body);
                    p.insert("append".into(), json!(append));
                    insert_opt(&mut p, "folder", folder);
                    finish(call(&mut h, "notes_update", Value::Object(p))?)
                }
                NotesCmd::Delete { id, yes } => finish(call(
                    &mut h,
                    "notes_delete",
                    json!({ "id": id, "confirm": yes }),
                )?),
            }
        }

        Cmd::Contacts { cmd } => {
            let mut h = BridgeHandle::new();
            match cmd {
                ContactsCmd::Search { query, limit } => {
                    let mut p = Map::new();
                    p.insert("query".into(), json!(query));
                    insert_opt(&mut p, "limit", limit);
                    finish(call(&mut h, "contacts_search", Value::Object(p))?)
                }
                ContactsCmd::Get { id } => {
                    finish(call(&mut h, "contacts_get", json!({ "id": id }))?)
                }
                ContactsCmd::Create {
                    first,
                    last,
                    org,
                    note,
                    emails,
                    phones,
                } => {
                    let mut p = Map::new();
                    insert_opt(&mut p, "first", first);
                    insert_opt(&mut p, "last", last);
                    insert_opt(&mut p, "org", org);
                    insert_opt(&mut p, "note", note);
                    if !emails.is_empty() {
                        p.insert("emails".into(), json!(parse_label_values(&emails)));
                    }
                    if !phones.is_empty() {
                        p.insert("phones".into(), json!(parse_label_values(&phones)));
                    }
                    finish(call(&mut h, "contacts_create", Value::Object(p))?)
                }
                ContactsCmd::Update {
                    id,
                    first,
                    last,
                    org,
                } => {
                    let mut p = Map::new();
                    p.insert("id".into(), json!(id));
                    insert_opt(&mut p, "first", first);
                    insert_opt(&mut p, "last", last);
                    insert_opt(&mut p, "org", org);
                    finish(call(&mut h, "contacts_update", Value::Object(p))?)
                }
                ContactsCmd::Delete { id, yes } => finish(call(
                    &mut h,
                    "contacts_delete",
                    json!({ "id": id, "confirm": yes }),
                )?),
                ContactsCmd::Groups => finish(call(&mut h, "contacts_groups", json!({}))?),
            }
        }

        Cmd::Messages { cmd } => {
            let mut h = BridgeHandle::new();
            match cmd {
                MessagesCmd::Send { to, text } => finish(call(
                    &mut h,
                    "messages_send",
                    json!({ "to": to, "text": text }),
                )?),
                MessagesCmd::Chats { limit } => {
                    let mut p = Map::new();
                    insert_opt(&mut p, "limit", limit);
                    finish(call(&mut h, "messages_chats_recent", Value::Object(p))?)
                }
                MessagesCmd::History { chat_id, limit } => {
                    let mut p = Map::new();
                    p.insert("chat_id".into(), json!(chat_id));
                    insert_opt(&mut p, "limit", limit);
                    finish(call(&mut h, "messages_history", Value::Object(p))?)
                }
            }
        }
    }
}

/// Execute a registry tool through the bridge; retries once if the helper died.
fn call(handle: &mut BridgeHandle, tool: &str, params: Value) -> Result<Value, BiteError> {
    match ops::run(handle.get()?, tool, params.clone()) {
        Ok(v) => Ok(v),
        Err(e) if e.code == "helper_exited" => {
            handle.reset();
            ops::run(handle.get()?, tool, params).map_err(BiteError)
        }
        Err(e) => Err(BiteError(e)),
    }
}

fn insert_opt<T: Into<Value>>(map: &mut Map<String, Value>, key: &str, value: Option<T>) {
    if let Some(v) = value {
        map.insert(key.into(), v.into());
    }
}

fn insert_date(
    map: &mut Map<String, Value>,
    key: &str,
    value: Option<String>,
) -> Result<(), BiteError> {
    if let Some(s) = value {
        map.insert(key.into(), json!(parse_date(&s)?));
    }
    Ok(())
}

/// CLI dates accept ISO or relaxed forms; the protocol takes RFC 3339.
pub fn parse_date(s: &str) -> Result<String, BiteError> {
    bite_core::dates::parse_to_rfc3339(s).map_err(cli_error)
}

/// "work=a@b.c" → {"label":"work","value":"a@b.c"}
fn parse_label_values(items: &[String]) -> Vec<Value> {
    items
        .iter()
        .map(|s| match s.split_once('=') {
            Some((label, value)) => json!({"label": label, "value": value}),
            None => json!({"label": "other", "value": s}),
        })
        .collect()
}
