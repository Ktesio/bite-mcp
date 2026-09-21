//! Agent-CLI MCP config writers. One entry per client: detect, add, remove,
//! status. Writers are surgical — they read the existing config, touch only
//! the bite entry, keep everything else byte-compatible (JSON pretty / TOML
//! via toml_edit), and leave a `.bite-bak` backup before the first change.

use std::path::PathBuf;

use serde_json::{json, Value};

pub const SERVER_KEY: &str = "bite";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

fn bite_entry() -> Value {
    json!({ "command": "bite", "args": ["mcp"] })
}

fn codex_bite_entry() -> Value {
    json!({ "command": "bite", "args": ["mcp"] })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKind {
    /// {"mcpServers": {"bite": {...}}}  (Claude, ZCode, Cursor, Gemini)
    McpServers,
    /// {"servers": {"bite": {...}}}     (VS Code mcp.json)
    Servers,
    /// {"mcp": {"bite": {"type":"local","command":[...]}}}  (OpenCode)
    OpenCode,
    /// TOML [mcp_servers.bite]          (Codex)
    CodexToml,
}

pub struct ClientSpec {
    pub key: &'static str,
    pub display: &'static str,
    /// Binary hints for detection (any hit = detected).
    pub binaries: &'static [&'static str],
    /// Candidate config paths, in priority order.
    pub paths: Vec<PathBuf>,
    pub kind: ConfigKind,
}

/// Path literals are evaluated eagerly in a static — use functions instead.
pub fn clients() -> Vec<ClientSpec> {
    let home = home();
    let app_support = dirs::data_dir().unwrap_or_else(|| home.join("Library/Application Support"));
    vec![
        ClientSpec {
            key: "claude",
            display: "Claude Code",
            binaries: &["claude"],
            paths: vec![home.join(".claude.json")],
            kind: ConfigKind::McpServers,
        },
        ClientSpec {
            key: "zcode",
            display: "ZCode",
            binaries: &["zcode", "z"],
            paths: vec![
                home.join(".zcode").join("settings.json"),
                home.join(".zcode").join("config.json"),
            ],
            kind: ConfigKind::McpServers,
        },
        ClientSpec {
            key: "codex",
            display: "Codex CLI",
            binaries: &["codex"],
            paths: vec![home.join(".codex").join("config.toml")],
            kind: ConfigKind::CodexToml,
        },
        ClientSpec {
            key: "opencode",
            display: "OpenCode",
            binaries: &["opencode"],
            paths: vec![dirs::config_dir()
                .unwrap_or_else(|| home.join(".config"))
                .join("opencode")
                .join("opencode.json")],
            kind: ConfigKind::OpenCode,
        },
        ClientSpec {
            key: "cursor",
            display: "Cursor",
            binaries: &["cursor-agent", "cursor"],
            paths: vec![home.join(".cursor").join("mcp.json")],
            kind: ConfigKind::McpServers,
        },
        ClientSpec {
            key: "vscode",
            display: "VS Code",
            binaries: &["code"],
            paths: vec![app_support.join("Code").join("User").join("mcp.json")],
            kind: ConfigKind::Servers,
        },
        ClientSpec {
            key: "gemini",
            display: "Gemini CLI",
            binaries: &["gemini"],
            paths: vec![home.join(".gemini").join("settings.json")],
            kind: ConfigKind::McpServers,
        },
    ]
}

impl ClientSpec {
    pub fn config_path(&self) -> PathBuf {
        for p in &self.paths {
            if p.exists() {
                return p.clone();
            }
        }
        self.paths[0].clone()
    }

    pub fn detect(&self) -> bool {
        self.paths.iter().any(|p| p.exists()) || self.binaries.iter().any(|b| which(b))
    }

    pub fn installed(&self) -> bool {
        let path = self.config_path();
        match self.kind {
            ConfigKind::CodexToml => std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.parse::<toml_edit::DocumentMut>().ok())
                .map(|doc| {
                    doc.get("mcp_servers")
                        .and_then(|m| m.get(SERVER_KEY))
                        .is_some()
                })
                .unwrap_or(false),
            kind => std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .map(|v| match kind {
                    ConfigKind::OpenCode => v["mcp"][SERVER_KEY].is_object(),
                    ConfigKind::Servers => v["servers"][SERVER_KEY].is_object(),
                    _ => v["mcpServers"][SERVER_KEY].is_object(),
                })
                .unwrap_or(false),
        }
    }

    /// Add (or refresh) the bite entry. Idempotent.
    pub fn add(&self) -> Result<PathBuf, String> {
        let path = self.config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        match self.kind {
            ConfigKind::CodexToml => self.add_toml(&path)?,
            ConfigKind::OpenCode => self.add_json(&path, |root| {
                root["mcp"][SERVER_KEY] = json!({
                    "type": "local",
                    "command": ["bite", "mcp"],
                });
            })?,
            ConfigKind::Servers => self.add_json(&path, |root| {
                root["servers"][SERVER_KEY] = bite_entry();
            })?,
            ConfigKind::McpServers => self.add_json(&path, |root| {
                root["mcpServers"][SERVER_KEY] = bite_entry();
            })?,
        }
        Ok(path)
    }

    fn add_json(&self, path: &PathBuf, mutate: impl FnOnce(&mut Value)) -> Result<(), String> {
        let mut root: Value = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({}));
        backup_once(path, &root)?;
        mutate(&mut root);
        let text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
        std::fs::write(path, text + "\n").map_err(|e| e.to_string())
    }

    fn add_toml(&self, path: &PathBuf) -> Result<(), String> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("{path:?} is not valid TOML: {e}"))?;
        backup_once_text(path, &text)?;
        let entry = codex_bite_entry();
        let command = entry["command"].as_str().unwrap().to_string();
        let args: Vec<String> = entry["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut table = toml_edit::Table::new();
        table.insert("command", toml_edit::value(command));
        let mut arr = toml_edit::Array::new();
        for a in args {
            arr.push(a);
        }
        table.insert("args", toml_edit::value(arr));
        doc["mcp_servers"][SERVER_KEY] = toml_edit::Item::Table(table);
        std::fs::write(path, doc.to_string()).map_err(|e| e.to_string())
    }

    #[allow(dead_code)]
    pub fn remove(&self) -> Result<bool, String> {
        let path = self.config_path();
        if !path.exists() {
            return Ok(false);
        }
        match self.kind {
            ConfigKind::CodexToml => {
                let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                let mut doc = text
                    .parse::<toml_edit::DocumentMut>()
                    .map_err(|e| e.to_string())?;
                let removed = doc
                    .get_mut("mcp_servers")
                    .and_then(|m| m.as_table_mut())
                    .map(|t| t.remove(SERVER_KEY).is_some())
                    .unwrap_or(false);
                if removed {
                    std::fs::write(&path, doc.to_string()).map_err(|e| e.to_string())?;
                }
                Ok(removed)
            }
            kind => {
                let mut root: Value = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_else(|| json!({}));
                let removed = match kind {
                    ConfigKind::OpenCode => root["mcp"]
                        .as_object_mut()
                        .map(|m| m.remove(SERVER_KEY).is_some()),
                    ConfigKind::Servers => root["servers"]
                        .as_object_mut()
                        .map(|m| m.remove(SERVER_KEY).is_some()),
                    _ => root["mcpServers"]
                        .as_object_mut()
                        .map(|m| m.remove(SERVER_KEY).is_some()),
                }
                .unwrap_or(false);
                if removed {
                    let text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
                    std::fs::write(&path, text + "\n").map_err(|e| e.to_string())?;
                }
                Ok(removed)
            }
        }
    }
}

fn backup_once(path: &std::path::Path, current: &Value) -> Result<(), String> {
    backup_once_text(
        path,
        &serde_json::to_string_pretty(current).unwrap_or_default(),
    )
}

fn backup_once_text(path: &std::path::Path, text: &str) -> Result<(), String> {
    let bak = path.with_extension("bite-bak");
    if !bak.exists() {
        std::fs::write(&bak, text).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn which(bin: &str) -> bool {
    std::env::var("PATH")
        .ok()
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(bin))
                .any(|p| p.is_file())
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clients_cover_all_seven() {
        let all = clients();
        let keys: Vec<_> = all.iter().map(|c| c.key).collect();
        for k in [
            "claude", "zcode", "codex", "opencode", "cursor", "vscode", "gemini",
        ] {
            assert!(keys.contains(&k), "missing client {k}");
        }
    }
}
