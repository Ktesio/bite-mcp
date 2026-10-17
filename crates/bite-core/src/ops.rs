//! Operation execution: registry name + params JSON → bridge call.
//! This is the single funnel for both the MCP server and the CLI.

use bite_bridge::{Bridge, BridgeError};

use crate::config::Config;
use crate::registry::find;

/// Inject config defaults, then execute. Uses the process-lifetime cached
/// config (`Config::cached`) — this runs on EVERY tool call and the config
/// file cannot change under a long-lived server process (`bite config set`
/// is a CLI-only verb in its own process).
pub fn run(
    bridge: &Bridge,
    tool_name: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, BridgeError> {
    run_with_config(bridge, tool_name, params, &Config::cached())
}

/// Explicit per-call timeout (shorter budgets for Mail live fallbacks).
/// Only the timeout comes from the caller; the remaining defaults still
/// come from the cached config.
pub fn run_with_timeout(
    bridge: &Bridge,
    tool_name: &str,
    params: serde_json::Value,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, BridgeError> {
    run_with_config(
        bridge,
        tool_name,
        params,
        &Config {
            timeout_secs: Some(timeout.as_secs()),
            ..Config::cached()
        },
    )
}

/// Explicit-config entry point: the caller's `config` wins verbatim for
/// every default. Tests and specialized paths depend on this NOT consulting
/// the cache.
pub fn run_with_config(
    bridge: &Bridge,
    tool_name: &str,
    params: serde_json::Value,
    config: &Config,
) -> Result<serde_json::Value, BridgeError> {
    let tool = find(tool_name)
        .ok_or_else(|| BridgeError::new("unknown_tool", format!("no such tool: {tool_name}")))?;
    let params = apply_config_defaults(tool, params, config);
    bridge.call_timeout(tool.method, &params, config.timeout())
}

/// Inject config defaults into `params` (pure, unit-testable): calendar
/// create/search tools get the configured default calendar, mail tools the
/// configured default account — but only when the caller did not pass one
/// and the config actually carries a default. Non-object params are
/// normalized to an empty object. The explicit `config` is used verbatim.
fn apply_config_defaults(
    tool: &crate::registry::Tool,
    mut params: serde_json::Value,
    config: &Config,
) -> serde_json::Value {
    if !params.is_object() {
        params = serde_json::Map::<String, serde_json::Value>::new().into();
    }
    if let Some(obj) = params.as_object_mut() {
        // calendar default
        if (tool.method.starts_with("calendar.event_create")
            || tool.method.starts_with("calendar.events_search"))
            && !obj.contains_key("calendar")
            && !obj.contains_key("calendar_id")
        {
            if let Some(cal) = &config.default_calendar {
                obj.insert("calendar".into(), serde_json::Value::String(cal.clone()));
            }
        }
        // mail account default
        if tool.method.starts_with("mail.") && !obj.contains_key("account") {
            if let Some(acct) = &config.default_mail_account {
                obj.insert("account".into(), serde_json::Value::String(acct.clone()));
            }
        }
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{json_schema, tools};

    #[test]
    fn unknown_tool_is_an_error() {
        // run() would need a bridge; test the lookup path via find()
        assert!(find("nope").is_none());
        assert!(find("calendar_event_create").is_some());
    }

    #[test]
    fn apply_config_defaults_injects_only_where_expected() {
        use serde_json::json;
        let cfg = Config {
            default_calendar: Some("Home".into()),
            default_mail_account: Some("work".into()),
            ..Default::default()
        };
        let tool = find;

        // calendar default injects into params lacking it (create + search)
        let p = apply_config_defaults(
            tool("calendar_event_create").unwrap(),
            json!({ "title": "x" }),
            &cfg,
        );
        assert_eq!(p["calendar"], "Home");
        let p = apply_config_defaults(
            tool("calendar_events_search").unwrap(),
            json!({ "text": "x" }),
            &cfg,
        );
        assert_eq!(p["calendar"], "Home");
        // params already carrying `calendar` / `calendar_id` are untouched
        let p = apply_config_defaults(
            tool("calendar_event_create").unwrap(),
            json!({ "calendar": "Mine" }),
            &cfg,
        );
        assert_eq!(p["calendar"], "Mine");
        let p = apply_config_defaults(
            tool("calendar_events_search").unwrap(),
            json!({ "calendar_id": "7" }),
            &cfg,
        );
        assert!(p.get("calendar").is_none());
        // mail default injects, and an explicit account is left alone
        let p = apply_config_defaults(tool("mail_send").unwrap(), json!({ "to": ["a@b.c"] }), &cfg);
        assert_eq!(p["account"], "work");
        let p = apply_config_defaults(
            tool("mail_send").unwrap(),
            json!({ "account": "personal" }),
            &cfg,
        );
        assert_eq!(p["account"], "personal");
        // tools outside calendar-create/search and mail are untouched
        let p = apply_config_defaults(
            tool("reminders_create").unwrap(),
            json!({ "title": "x" }),
            &cfg,
        );
        assert!(p.get("calendar").is_none() && p.get("account").is_none());
        // a fully-specified config is used verbatim (nothing to add anywhere)
        let p = apply_config_defaults(
            tool("calendar_event_create").unwrap(),
            json!({}),
            &Config {
                default_calendar: Some("Explicit".into()),
                default_mail_account: Some("acct".into()),
                timeout_secs: Some(3),
                release_repo: None,
                auto_refresh_hours: Some(48),
            },
        );
        assert_eq!(p["calendar"], "Explicit");
        // non-object params are normalized to an object (no crash, no inject)
        let p = apply_config_defaults(tool("mail_send").unwrap(), json!(null), &cfg);
        assert!(p.is_object());
    }

    #[test]
    fn full_surface_registered() {
        let names: Vec<_> = tools().iter().map(|t| t.name).collect();
        for app in [
            "calendar",
            "reminders",
            "mail",
            "notes",
            "contacts",
            "messages",
        ] {
            assert!(
                names.iter().any(|n| n.starts_with(app)),
                "missing tools for {app}"
            );
        }
        assert!(names.contains(&"calendar_availability"));
        assert!(names.contains(&"mail_attachment_save"));
        assert!(names.contains(&"messages_history"));
        let _ = json_schema(&tools()[0]);
    }
}
