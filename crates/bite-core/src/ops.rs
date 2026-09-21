//! Operation execution: registry name + params JSON → bridge call.
//! This is the single funnel for both the MCP server and the CLI.

use bite_bridge::{Bridge, BridgeError};

use crate::config::Config;
use crate::registry::find;

/// Inject config defaults, then execute.
pub fn run(
    bridge: &Bridge,
    tool_name: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, BridgeError> {
    run_with_config(bridge, tool_name, params, &Config::load())
}

pub fn run_with_config(
    bridge: &Bridge,
    tool_name: &str,
    mut params: serde_json::Value,
    config: &Config,
) -> Result<serde_json::Value, BridgeError> {
    let tool = find(tool_name)
        .ok_or_else(|| BridgeError::new("unknown_tool", format!("no such tool: {tool_name}")))?;
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
    bridge.call_timeout(tool.method, &params, config.timeout())
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
