//! Agent-CLI MCP config writers. One entry per client: detect, add, remove,
//! status.
//!
//! Safety contract — a broken writer must never cost the user their config:
//!
//! - **Never clobber what we can't parse.** A non-empty config that fails to
//!   parse (JSONC comments in a strict-JSON file, trailing commas, hand
//!   edits) aborts that client with an error naming the file. We only start
//!   from `{}` when the file is absent or empty.
//! - **Atomic writes only.** Every write goes to a unique temp file in the
//!   target's own directory, is fsynced, then renamed over the target — an
//!   interrupted `bite setup` can never leave a truncated/0-byte config
//!   behind (the target is always the complete old or complete new file).
//! - **Backups keep the full file name** (`opencode.jsonc.bite-bak`, not
//!   `opencode.bite-bak`) and are taken from the original bytes before the
//!   first modification of a non-empty file. The first backup is never
//!   overwritten.
//! - **JSONC is edited losslessly.** OpenCode configs (`opencode.json[c]`)
//!   are parsed with a comment-preserving CST and re-serialized unchanged
//!   outside the inserted key — comments and formatting survive.
//! - **Idempotent.** Adding twice yields identical bytes.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

pub const SERVER_KEY: &str = "bite";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

/// XDG config base (`$XDG_CONFIG_HOME`, else `~/.config`). OpenCode reads
/// `~/.config/opencode` on every platform including macOS — NOT
/// `~/Library/Application Support` (opencode.ai/docs/config, "Global": place
/// your global OpenCode config in `~/.config/opencode/opencode.json`).
fn xdg_config_home() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(".config"),
    }
}

/// Claude Code config dir (`$CLAUDE_CONFIG_DIR`, else `$HOME`); user- and
/// local-scope MCP servers live in `<dir>/.claude.json` under `mcpServers`
/// (code.claude.com/docs/en/mcp-quickstart).
fn claude_config_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home(),
    }
}

/// Codex config dir (`$CODEX_HOME`, else `~/.codex`); MCP servers are
/// `[mcp_servers.<id>]` tables in `<dir>/config.toml`
/// (developers.openai.com/codex/config-reference).
fn codex_config_dir() -> PathBuf {
    match std::env::var_os("CODEX_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(".codex"),
    }
}

/// `{"command": "bite", "args": ["mcp"]}` — Claude Code / ZCode / Cursor /
/// Gemini CLI shape, all of which use top-level `mcpServers`.
fn bite_entry() -> Value {
    json!({ "command": "bite", "args": ["mcp"] })
}

/// VS Code user-profile `mcp.json` shape: top-level `servers` with
/// `"type": "stdio"` + `command` + `args`
/// (code.visualstudio.com/docs/agent-customization/mcp-servers).
fn vscode_entry() -> Value {
    json!({ "type": "stdio", "command": "bite", "args": ["mcp"] })
}

/// OpenCode shape: top-level `mcp` with `"type": "local"` and the command
/// plus args as one array (opencode.ai/docs/config).
fn opencode_entry() -> Value {
    json!({ "type": "local", "command": ["bite", "mcp"] })
}

/// Same entry as a CST replacement value (jsonc-parser's `json!` macro
/// expands an unqualified `json!` internally, which would collide with
/// serde_json's `json` import in this module — so build it explicitly).
fn opencode_cst_entry() -> jsonc_parser::cst::CstInputValue {
    use jsonc_parser::cst::CstInputValue;
    CstInputValue::Object(vec![
        ("type".into(), CstInputValue::from("local")),
        (
            "command".into(),
            CstInputValue::Array(vec!["bite".into(), "mcp".into()]),
        ),
    ])
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
    /// Creation target when no candidate exists (defaults to `paths[0]`).
    /// OpenCode sets this: prefer editing whichever file exists (jsonc
    /// first — it's the user's chosen file when present), but create
    /// `opencode.json` when neither exists.
    pub create: Option<PathBuf>,
}

/// Path literals are evaluated eagerly in a static — use functions instead.
pub fn clients() -> Vec<ClientSpec> {
    let home = home();
    let app_support = dirs::data_dir().unwrap_or_else(|| home.join("Library/Application Support"));
    let opencode_dir = xdg_config_home().join("opencode");
    vec![
        ClientSpec {
            key: "claude",
            display: "Claude Code",
            binaries: &["claude"],
            paths: vec![claude_config_dir().join(".claude.json")],
            kind: ConfigKind::McpServers,
            create: None,
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
            create: None,
        },
        ClientSpec {
            key: "codex",
            display: "Codex CLI",
            binaries: &["codex"],
            paths: vec![codex_config_dir().join("config.toml")],
            kind: ConfigKind::CodexToml,
            create: None,
        },
        ClientSpec {
            key: "opencode",
            display: "OpenCode",
            binaries: &["opencode"],
            // JSONC candidate first: if the user picked opencode.jsonc (that's
            // where comments/instructions live), we edit their file, not a
            // second one. opencode.ai/docs/config: global config is
            // `~/.config/opencode/opencode.json`, JSON and JSONC formats are
            // both supported (and merged when both exist).
            paths: vec![
                opencode_dir.join("opencode.jsonc"),
                opencode_dir.join("opencode.json"),
            ],
            kind: ConfigKind::OpenCode,
            create: Some(opencode_dir.join("opencode.json")),
        },
        ClientSpec {
            key: "cursor",
            display: "Cursor",
            binaries: &["cursor-agent", "cursor"],
            paths: vec![home.join(".cursor").join("mcp.json")],
            kind: ConfigKind::McpServers,
            create: None,
        },
        ClientSpec {
            key: "vscode",
            display: "VS Code",
            binaries: &["code"],
            // User-profile mcp.json — what "MCP: Open User Configuration"
            // opens (code.visualstudio.com/docs/agent-customization/
            // mcp-servers); on macOS that is the app-support User folder.
            paths: vec![app_support.join("Code").join("User").join("mcp.json")],
            kind: ConfigKind::Servers,
            create: None,
        },
        ClientSpec {
            key: "gemini",
            display: "Gemini CLI",
            binaries: &["gemini"],
            paths: vec![home.join(".gemini").join("settings.json")],
            kind: ConfigKind::McpServers,
            create: None,
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
        self.create.clone().unwrap_or_else(|| self.paths[0].clone())
    }

    pub fn detect(&self) -> bool {
        self.paths.iter().any(|p| p.exists()) || self.binaries.iter().any(|b| which(b))
    }

    /// bite is registered in ANY candidate config file — OpenCode merges
    /// `opencode.json` and `opencode.jsonc`, so the entry may live in either.
    pub fn installed(&self) -> bool {
        self.paths.iter().any(|p| self.installed_in(p))
    }

    fn installed_in(&self, path: &Path) -> bool {
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        if text.trim().is_empty() {
            return false;
        }
        match self.kind {
            ConfigKind::CodexToml => text
                .parse::<toml_edit::DocumentMut>()
                .ok()
                .map(|doc| {
                    doc.get("mcp_servers")
                        .and_then(|m| m.get(SERVER_KEY))
                        .is_some()
                })
                .unwrap_or(false),
            // JSON files are sniffed with a JSONC-tolerant parse: users keep
            // comments in some of these (VS Code's mcp.json), and a tolerant
            // read-only check can only ever prevent a needless write.
            kind => jsonc_parser::parse_to_serde_value(&text, &jsonc_parse_options())
                .ok()
                .map(|v: Value| {
                    let key = match kind {
                        ConfigKind::OpenCode => "mcp",
                        ConfigKind::Servers => "servers",
                        _ => "mcpServers",
                    };
                    v[key][SERVER_KEY].is_object()
                })
                .unwrap_or(false),
        }
    }

    /// Add (or refresh) the bite entry. Idempotent. Nothing is written unless
    /// the existing file parsed cleanly (or was absent/empty).
    pub fn add(&self) -> Result<PathBuf, String> {
        let path = self.config_path();
        match self.kind {
            ConfigKind::CodexToml => self.add_toml(&path)?,
            ConfigKind::OpenCode => self.add_opencode(&path)?,
            ConfigKind::Servers => self.add_json(&path, "servers", vscode_entry())?,
            ConfigKind::McpServers => self.add_json(&path, "mcpServers", bite_entry())?,
        }
        Ok(path)
    }

    /// Strict-JSON edit: merge `root_key.bite` into the existing config,
    /// preserving every other key. Aborts without writing when a non-empty
    /// file fails strict JSON parsing.
    fn add_json(&self, path: &Path, root_key: &str, entry: Value) -> Result<(), String> {
        let mut root = match read_existing(path)? {
            Existing::Fresh => json!({}),
            Existing::Text(text) => {
                let root: Value = serde_json::from_str(&text)
                    .map_err(|e| refuse_unparseable(path, &e.to_string()))?;
                if !root.is_object() {
                    return Err(refuse_not_mergeable(
                        path,
                        "top-level JSON value is not an object",
                    ));
                }
                backup_once(path, &text)?;
                root
            }
        };
        set_bite_entry(path, &mut root, root_key, entry)?;
        let text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
        atomic_write(path, &format!("{text}\n"))
    }

    /// Codex edit: `[mcp_servers.bite]` via toml_edit (formatting-preserving);
    /// same refuse-to-clobber and atomic-write guarantees.
    fn add_toml(&self, path: &Path) -> Result<(), String> {
        let mut doc = match read_existing(path)? {
            Existing::Fresh => toml_edit::DocumentMut::new(),
            Existing::Text(text) => {
                let doc: toml_edit::DocumentMut = text
                    .parse::<toml_edit::DocumentMut>()
                    .map_err(|e| refuse_unparseable(path, &e.to_string()))?;
                backup_once(path, &text)?;
                doc
            }
        };
        let mut bite = toml_edit::Table::new();
        bite.insert("command", toml_edit::value("bite"));
        let mut args = toml_edit::Array::new();
        args.push("mcp");
        bite.insert("args", toml_edit::value(args));
        match doc.get_mut("mcp_servers") {
            // NB: never `doc["mcp_servers"][key] = …` — IndexMut auto-creates
            // mcp_servers as an INLINE table (`mcp_servers = { … }`), which
            // re-parses as a non-table and is not the documented form.
            Some(item) => {
                let Some(table) = item.as_table_mut() else {
                    return Err(refuse_not_mergeable(path, "`mcp_servers` is not a table"));
                };
                table.insert(SERVER_KEY, toml_edit::Item::Table(bite));
            }
            None => {
                let mut mcp = toml_edit::Table::new();
                // implicit: renders as `[mcp_servers.bite]`, no bare
                // `[mcp_servers]` header
                mcp.set_implicit(true);
                mcp.insert(SERVER_KEY, toml_edit::Item::Table(bite));
                doc.insert("mcp_servers", toml_edit::Item::Table(mcp));
            }
        }
        atomic_write(path, &doc.to_string())
    }

    /// OpenCode edit. `opencode.json[c]` may be JSONC — comments and
    /// formatting are preserved by editing a lossless CST and re-serializing;
    /// only the `mcp.bite` key is inserted/replaced.
    fn add_opencode(&self, path: &Path) -> Result<(), String> {
        match read_existing(path)? {
            Existing::Fresh => {
                let mut root = json!({ "mcp": {} });
                root["mcp"][SERVER_KEY] = opencode_entry();
                let text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
                atomic_write(path, &format!("{text}\n"))
            }
            Existing::Text(text) => {
                let root = jsonc_parser::cst::CstRootNode::parse(&text, &jsonc_parse_options())
                    .map_err(|e| refuse_unparseable(path, &e.to_string()))?;
                let root_obj = root.object_value().ok_or_else(|| {
                    refuse_not_mergeable(path, "top-level value is not an object")
                })?;
                backup_once(path, &text)?;
                let mcp = root_obj
                    .object_value_or_create("mcp")
                    .ok_or_else(|| refuse_not_mergeable(path, "`mcp` is not an object"))?;
                // Built explicitly: jsonc-parser's `json!` macro invokes an
                // unqualified `json!` internally, which collides with
                // serde_json's `json` import in this module.
                let entry = opencode_cst_entry();
                match mcp.get(SERVER_KEY) {
                    Some(prop) => prop.set_value(entry),
                    None => {
                        mcp.append(SERVER_KEY, entry);
                    }
                }
                atomic_write(path, &root.to_string())
            }
        }
    }

    /// Remove the bite entry from every candidate file that holds it. Same
    /// safety as add: abort on unparseable files, back up before modifying,
    /// atomic writes.
    #[allow(dead_code)]
    pub fn remove(&self) -> Result<bool, String> {
        let mut removed_any = false;
        for path in &self.paths {
            if !path.exists() {
                continue;
            }
            match self.kind {
                ConfigKind::CodexToml => removed_any |= self.remove_toml(path)?,
                ConfigKind::OpenCode => removed_any |= self.remove_jsonc(path)?,
                ConfigKind::Servers => removed_any |= self.remove_json(path, "servers")?,
                ConfigKind::McpServers => removed_any |= self.remove_json(path, "mcpServers")?,
            }
        }
        Ok(removed_any)
    }

    fn remove_json(&self, path: &Path, root_key: &str) -> Result<bool, String> {
        let text = match read_existing(path)? {
            Existing::Fresh => return Ok(false),
            Existing::Text(t) => t,
        };
        let mut root: Value =
            serde_json::from_str(&text).map_err(|e| refuse_unparseable(path, &e.to_string()))?;
        let removed = root
            .get_mut(root_key)
            .and_then(Value::as_object_mut)
            .map(|m| m.remove(SERVER_KEY).is_some())
            .unwrap_or(false);
        if removed {
            backup_once(path, &text)?;
            let out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
            atomic_write(path, &format!("{out}\n"))?;
        }
        Ok(removed)
    }

    fn remove_toml(&self, path: &Path) -> Result<bool, String> {
        let text = match read_existing(path)? {
            Existing::Fresh => return Ok(false),
            Existing::Text(t) => t,
        };
        let mut doc: toml_edit::DocumentMut = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| refuse_unparseable(path, &e.to_string()))?;
        let removed = doc
            .get_mut("mcp_servers")
            .and_then(|m| m.as_table_mut())
            .map(|t| t.remove(SERVER_KEY).is_some())
            .unwrap_or(false);
        if removed {
            backup_once(path, &text)?;
            atomic_write(path, &doc.to_string())?;
        }
        Ok(removed)
    }

    fn remove_jsonc(&self, path: &Path) -> Result<bool, String> {
        let text = match read_existing(path)? {
            Existing::Fresh => return Ok(false),
            Existing::Text(t) => t,
        };
        let root = jsonc_parser::cst::CstRootNode::parse(&text, &jsonc_parse_options())
            .map_err(|e| refuse_unparseable(path, &e.to_string()))?;
        let root_obj = root
            .object_value()
            .ok_or_else(|| refuse_not_mergeable(path, "top-level value is not an object"))?;
        let prop = root_obj.object_value("mcp").and_then(|m| m.get(SERVER_KEY));
        let Some(prop) = prop else {
            return Ok(false);
        };
        backup_once(path, &text)?;
        prop.remove();
        atomic_write(path, &root.to_string())?;
        Ok(true)
    }
}

fn set_bite_entry(
    path: &Path,
    root: &mut Value,
    root_key: &str,
    entry: Value,
) -> Result<(), String> {
    let obj = root
        .as_object_mut()
        .ok_or_else(|| refuse_not_mergeable(path, "top-level JSON value is not an object"))?;
    let section = obj.entry(root_key).or_insert_with(|| json!({}));
    if !section.is_object() {
        return Err(refuse_not_mergeable(
            path,
            &format!("`{root_key}` is not an object"),
        ));
    }
    section[SERVER_KEY] = entry;
    Ok(())
}

/// What we found on disk for a config path.
enum Existing {
    /// Absent or empty (0 bytes / whitespace only): safe to start from
    /// scratch — there is nothing to lose.
    Fresh,
    /// Non-empty text: the user's data. Parse it or refuse to touch it.
    Text(String),
}

fn read_existing(path: &Path) -> Result<Existing, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Existing::Fresh),
        Ok(text) => Ok(Existing::Text(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Existing::Fresh),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn refuse_unparseable(path: &Path, parse_error: &str) -> String {
    format!(
        "{} is not parseable ({parse_error}); nothing was written — fix its syntax \
         or merge the bite entry by hand, then re-run `bite setup`",
        path.display()
    )
}

fn refuse_not_mergeable(path: &Path, what: &str) -> String {
    format!(
        "{}: {what}; nothing was written — merge the bite entry by hand",
        path.display()
    )
}

/// JSONC semantics only: comments and trailing commas allowed, nothing
/// looser — we accept what OpenCode accepts, no more.
fn jsonc_parse_options() -> jsonc_parser::ParseOptions {
    jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        ..Default::default()
    }
}

/// Distinct tmp names per invocation, even from parallel processes.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `contents` to `path` atomically: write to a unique temp file in the
/// target's own directory, fsync it, then `rename` over the target (atomic
/// within a directory). An interrupted write can never leave a truncated
/// config behind — the target is always the complete old or complete new
/// file, and a failed write removes its temp file and leaves the target
/// untouched.
fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: not a file path", path.display()))?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.bite-tmp-{}-{seq}", std::process::id()));

    let write_tmp = || -> Result<(), std::io::Error> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        drop(f);
        Ok(())
    };
    if let Err(e) = write_tmp() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    // Best-effort directory fsync so the rename itself is durable.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Backup path keeps the FULL file name (suffix style): `opencode.jsonc`
/// backs up to `opencode.jsonc.bite-bak` — `with_extension("bite-bak")`
/// would collide `opencode.json` and `opencode.jsonc` onto the same
/// `opencode.bite-bak`.
fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.bite-bak"))
}

/// Copy the ORIGINAL bytes of a non-empty config aside, once. Never
/// overwrites an existing backup: the first one — the state before bite ever
/// touched the file — is the one worth keeping.
fn backup_once(path: &Path, original: &str) -> Result<(), String> {
    let bak = backup_path(path);
    if bak.exists() {
        return Ok(());
    }
    atomic_write(&bak, original)
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

    /// Unique scratch dir per test (tests run in parallel in one process).
    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bite-clients-tests-{name}-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spec_in(dir: &Path, name: &str, kind: ConfigKind) -> ClientSpec {
        ClientSpec {
            key: "test",
            display: "Test",
            binaries: &[],
            paths: vec![dir.join(name)],
            kind,
            create: None,
        }
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn jsonc_value(text: &str) -> Value {
        jsonc_parser::parse_to_serde_value(text, &jsonc_parse_options()).expect("parseable jsonc")
    }

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

    #[test]
    fn opencode_targets_xdg_config_with_jsonc_candidate() {
        let spec = clients().into_iter().find(|c| c.key == "opencode").unwrap();
        let names: Vec<String> = spec
            .paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["opencode.jsonc", "opencode.json"]);
        // never ~/Library/Application Support (dirs::config_dir on macOS)
        for p in &spec.paths {
            assert!(
                !p.starts_with(home().join("Library/Application Support")),
                "opencode must live under ~/.config (XDG), not {}",
                p.display()
            );
        }
        // creation default keeps the plain .json name
        let create = spec.create.expect("opencode sets a creation target");
        assert_eq!(
            create.file_name().unwrap().to_string_lossy(),
            "opencode.json"
        );
        assert_eq!(create.parent(), spec.paths[0].parent());
    }

    #[test]
    fn config_path_prefers_existing_jsonc_and_creates_plain_json() {
        let dir = tempdir("cfgpath");
        let jsonc = dir.join("opencode.jsonc");
        let json = dir.join("opencode.json");
        let spec = ClientSpec {
            key: "opencode",
            display: "OpenCode",
            binaries: &[],
            paths: vec![jsonc.clone(), json.clone()],
            kind: ConfigKind::OpenCode,
            create: Some(json.clone()),
        };
        // neither exists → create plain json
        assert_eq!(spec.config_path(), json);
        // only json exists → edit it
        std::fs::write(&json, "{}").unwrap();
        assert_eq!(spec.config_path(), json);
        // jsonc appears → the user's chosen file wins
        std::fs::write(&jsonc, "{}").unwrap();
        assert_eq!(spec.config_path(), jsonc);
        // default for other clients stays paths[0]
        let plain = spec_in(&dir, "settings.json", ConfigKind::McpServers);
        assert_eq!(plain.config_path(), dir.join("settings.json"));
    }

    // ── merge safety (strict JSON clients) ───────────────────────────────

    #[test]
    fn add_json_preserves_unknown_keys() {
        let dir = tempdir("merge");
        let path = dir.join("mcp.json");
        std::fs::write(
            &path,
            r#"{"first":"kept","mcpServers":{"other":{"command":"x"}}}"#,
        )
        .unwrap();
        let spec = spec_in(&dir, "mcp.json", ConfigKind::McpServers);
        spec.add().unwrap();
        let v: Value = serde_json::from_str(&read(&path)).unwrap();
        assert_eq!(v["first"], "kept");
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"][SERVER_KEY]["command"], "bite");
        assert_eq!(v["mcpServers"][SERVER_KEY]["args"], json!(["mcp"]));
    }

    #[test]
    fn add_json_unparseable_file_aborts_untouched() {
        let dir = tempdir("unparseable");
        let path = dir.join("mcp.json");
        // JSONC comments in a strict-JSON slot + a hand-edit: must abort.
        let original = "{\n  // my servers\n  broken\n";
        std::fs::write(&path, original).unwrap();
        let spec = spec_in(&dir, "mcp.json", ConfigKind::McpServers);
        let err = spec.add().unwrap_err();
        assert!(err.contains("mcp.json"), "error names the file: {err}");
        assert!(
            err.contains("nothing was written"),
            "explains refusal: {err}"
        );
        assert_eq!(read(&path), original, "file byte-identical after refusal");
        assert!(
            !backup_path(&path).exists(),
            "no backup of a file we refused"
        );
    }

    #[test]
    fn add_json_empty_file_gets_fresh_config_only() {
        let dir = tempdir("empty");
        for seed in ["", " \n\t"] {
            let path = dir.join(format!("mcp-{:?}.json", seed.len()));
            std::fs::write(&path, seed).unwrap();
            let spec = spec_in(
                &dir,
                path.file_name().unwrap().to_str().unwrap(),
                ConfigKind::McpServers,
            );
            spec.add().unwrap();
            let v: Value = serde_json::from_str(&read(&path)).unwrap();
            let obj = v.as_object().unwrap();
            assert_eq!(obj.len(), 1, "only mcpServers in {seed:?}");
            assert!(v["mcpServers"][SERVER_KEY].is_object());
            assert!(!backup_path(&path).exists(), "nothing to back up");
        }
    }

    #[test]
    fn add_json_absent_file_gets_fresh_config_only() {
        let dir = tempdir("absent");
        let path = dir.join("nested/deep/mcp.json");
        let spec = spec_in(&dir, "nested/deep/mcp.json", ConfigKind::McpServers);
        let written = spec.add().unwrap();
        assert_eq!(written, path);
        let v: Value = serde_json::from_str(&read(&path)).unwrap();
        assert!(v["mcpServers"][SERVER_KEY].is_object());
        assert!(!backup_path(&path).exists(), "nothing to back up");
    }

    #[test]
    fn add_json_section_of_wrong_type_refuses() {
        let dir = tempdir("wrongtype");
        let path = dir.join("mcp.json");
        std::fs::write(&path, r#"{"mcpServers": 42}"#).unwrap();
        let spec = spec_in(&dir, "mcp.json", ConfigKind::McpServers);
        let err = spec.add().unwrap_err();
        assert!(err.contains("`mcpServers` is not an object"), "{err}");
        assert_eq!(read(&path), r#"{"mcpServers": 42}"#);
    }

    #[test]
    fn vscode_entry_matches_current_documented_shape() {
        let dir = tempdir("vscode");
        let path = dir.join("mcp.json");
        let spec = spec_in(&dir, "mcp.json", ConfigKind::Servers);
        spec.add().unwrap();
        let v: Value = serde_json::from_str(&read(&path)).unwrap();
        assert_eq!(v["servers"][SERVER_KEY]["type"], "stdio");
        assert_eq!(v["servers"][SERVER_KEY]["command"], "bite");
        assert_eq!(v["servers"][SERVER_KEY]["args"], json!(["mcp"]));
    }

    #[test]
    fn add_is_idempotent_across_all_kinds() {
        let dir = tempdir("idem");
        let cases: &[(&str, ConfigKind, &str)] = &[
            ("claude.json", ConfigKind::McpServers, r#"{"a":1}"#),
            ("vscode.json", ConfigKind::Servers, r#"{"servers":{}}"#),
            ("opencode.json", ConfigKind::OpenCode, r#"{"a":1}"#),
            ("codex.toml", ConfigKind::CodexToml, "other = 1\n"),
        ];
        for (name, kind, seed) in cases {
            let path = dir.join(format!("idem-{name}"));
            std::fs::write(&path, seed).unwrap();
            let spec = ClientSpec {
                key: "test",
                display: "T",
                binaries: &[],
                paths: vec![path.clone()],
                kind: *kind,
                create: None,
            };
            spec.add().unwrap();
            let first = read(&path);
            spec.add().unwrap();
            let second = read(&path);
            assert_eq!(first, second, "{name}: second add changed the file");
        }
    }

    // ── backups ──────────────────────────────────────────────────────────

    #[test]
    fn backup_keeps_full_filename_only_from_nonempty_original_and_never_overwrites() {
        let dir = tempdir("backup");
        let path = dir.join("opencode.jsonc");
        let original = "{\n  // my config\n}\n";
        std::fs::write(&path, original).unwrap();
        let spec = spec_in(&dir, "opencode.jsonc", ConfigKind::OpenCode);
        spec.add().unwrap();

        let bak = backup_path(&path);
        assert_eq!(
            bak.file_name().unwrap(),
            "opencode.jsonc.bite-bak",
            "full filename preserved"
        );
        assert_eq!(read(&bak), original, "backup holds the original bytes");

        // a later modification never replaces the first backup
        std::fs::write(&bak, "PRECIOUS-FIRST-BACKUP").unwrap();
        std::fs::write(&path, "{\n  // my config\n  \"more\": true\n}\n").unwrap();
        spec.add().unwrap();
        assert_eq!(read(&bak), "PRECIOUS-FIRST-BACKUP");
    }

    #[test]
    fn backup_names_do_not_collide_across_json_and_jsonc() {
        let dir = tempdir("baknames");
        let json = dir.join("opencode.json");
        let jsonc = dir.join("opencode.jsonc");
        assert_ne!(backup_path(&json), backup_path(&jsonc));
    }

    // ── OpenCode JSONC lossless editing ──────────────────────────────────

    #[test]
    fn opencode_jsonc_comments_and_formatting_survive_the_edit() {
        let dir = tempdir("jsonc");
        let path = dir.join("opencode.jsonc");
        let original = "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  // my instructions live here\n  \"instructions\": [\"MEMORY.md\"]\n}\n";
        std::fs::write(&path, original).unwrap();
        let spec = spec_in(&dir, "opencode.jsonc", ConfigKind::OpenCode);
        spec.add().unwrap();
        let out = read(&path);
        // every original line survives byte-for-byte (the last property line
        // may legitimately gain the comma the insertion requires)
        for line in original.lines() {
            if line.trim().is_empty() {
                continue;
            }
            assert!(
                out.contains(line) || out.contains(&format!("{line},")),
                "original line lost: {line:?}\n--- got ---\n{out}"
            );
        }
        assert!(out.contains("// my instructions live here"));
        let v = jsonc_value(&out);
        assert_eq!(v["$schema"], "https://opencode.ai/config.json");
        assert_eq!(v["instructions"], json!(["MEMORY.md"]));
        assert_eq!(v["mcp"][SERVER_KEY]["type"], "local");
        assert_eq!(v["mcp"][SERVER_KEY]["command"], json!(["bite", "mcp"]));
    }

    #[test]
    fn opencode_jsonc_refresh_replaces_only_the_bite_entry() {
        let dir = tempdir("jsonc2");
        let path = dir.join("opencode.jsonc");
        let original = "{\n  // top\n  \"mcp\": {\n    // inner comment\n    \"bite\": {\"type\": \"local\", \"command\": [\"wrong\", \"cmd\"]},\n    \"other\": {\"type\": \"local\", \"command\": [\"x\"]}\n  }\n}\n";
        std::fs::write(&path, original).unwrap();
        let spec = spec_in(&dir, "opencode.jsonc", ConfigKind::OpenCode);
        spec.add().unwrap();
        let out = read(&path);
        assert!(out.contains("// top"));
        assert!(out.contains("// inner comment"));
        assert!(!out.contains("wrong"), "stale entry replaced");
        let v = jsonc_value(&out);
        assert_eq!(v["mcp"][SERVER_KEY]["command"], json!(["bite", "mcp"]));
        assert_eq!(v["mcp"]["other"]["command"], json!(["x"]));
    }

    #[test]
    fn opencode_unparseable_jsonc_aborts_untouched() {
        let dir = tempdir("jsoncbad");
        let path = dir.join("opencode.jsonc");
        let original = "{\n  // fine comment\n  broken beyond jsonc: [[[ }\n";
        std::fs::write(&path, original).unwrap();
        let spec = spec_in(&dir, "opencode.jsonc", ConfigKind::OpenCode);
        let err = spec.add().unwrap_err();
        assert!(err.contains("opencode.jsonc"), "{err}");
        assert!(err.contains("nothing was written"), "{err}");
        assert_eq!(read(&path), original);
    }

    #[test]
    fn opencode_existing_plain_json_edited_losslessly() {
        let dir = tempdir("ocjson");
        let path = dir.join("opencode.json");
        let original = "{\n  \"$schema\": \"https://opencode.ai/config.json\"\n}\n";
        std::fs::write(&path, original).unwrap();
        let spec = spec_in(&dir, "opencode.json", ConfigKind::OpenCode);
        spec.add().unwrap();
        let out = read(&path);
        assert!(out.contains("\"$schema\""));
        let v = jsonc_value(&out);
        assert_eq!(v["mcp"][SERVER_KEY]["type"], "local");
    }

    #[test]
    fn installed_detects_entry_in_whichever_opencode_file_holds_it() {
        let dir = tempdir("ocdetect");
        let json = dir.join("opencode.json");
        let jsonc = dir.join("opencode.jsonc");
        let spec = ClientSpec {
            key: "opencode",
            display: "OpenCode",
            binaries: &[],
            paths: vec![jsonc.clone(), json.clone()],
            kind: ConfigKind::OpenCode,
            create: Some(json.clone()),
        };
        assert!(!spec.installed());
        // entry in the plain json
        std::fs::write(
            &json,
            r#"{"mcp":{"bite":{"type":"local","command":["bite","mcp"]}}}"#,
        )
        .unwrap();
        assert!(spec.installed(), "detected in opencode.json");
        // entry moved into a commented jsonc
        std::fs::remove_file(&json).unwrap();
        std::fs::write(
            &jsonc,
            "{\n  // comments allowed\n  \"mcp\": {\"bite\": {\"type\": \"local\", \"command\": [\"bite\", \"mcp\"]}}\n}",
        )
        .unwrap();
        assert!(spec.installed(), "detected in commented opencode.jsonc");
        // a different server only → not installed
        std::fs::write(&jsonc, r#"{"mcp":{"other":{}}}"#).unwrap();
        assert!(!spec.installed());
    }

    #[test]
    fn installed_tolerates_jsonc_in_strict_json_clients() {
        let dir = tempdir("detect");
        let path = dir.join("mcp.json");
        std::fs::write(
            &path,
            "{\n  // VS Code mcp.json files are edited as JSONC\n  \"servers\": {\"bite\": {\"type\": \"stdio\", \"command\": \"bite\"}}\n}",
        )
        .unwrap();
        let spec = spec_in(&dir, "mcp.json", ConfigKind::Servers);
        assert!(spec.installed());
    }

    // ── Codex TOML ───────────────────────────────────────────────────────

    #[test]
    fn add_toml_preserves_unknown_keys_and_aborts_on_garbage() {
        let dir = tempdir("toml");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "model = \"gpt-5\"\n\n[sandbox_workspace_write]\nnetwork_access = false\n",
        )
        .unwrap();
        let spec = spec_in(&dir, "config.toml", ConfigKind::CodexToml);
        spec.add().unwrap();
        let out = read(&path);
        let doc: toml_edit::DocumentMut = out.parse().unwrap();
        assert_eq!(doc["model"].as_str(), Some("gpt-5"));
        assert_eq!(
            doc["sandbox_workspace_write"]["network_access"].as_bool(),
            Some(false)
        );
        assert_eq!(
            doc["mcp_servers"][SERVER_KEY]["command"].as_str(),
            Some("bite")
        );
        let args: Vec<String> = doc["mcp_servers"][SERVER_KEY]["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        assert_eq!(args, vec!["mcp"]);

        // garbage TOML refuses
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, "not [valid toml\n").unwrap();
        let spec_bad = spec_in(&dir, "bad.toml", ConfigKind::CodexToml);
        let err = spec_bad.add().unwrap_err();
        assert!(err.contains("bad.toml"), "{err}");
        assert!(err.contains("nothing was written"), "{err}");
        assert_eq!(read(&bad), "not [valid toml\n");
    }

    // ── remove ───────────────────────────────────────────────────────────

    #[test]
    fn remove_json_is_safe_atomic_and_backed_up() {
        let dir = tempdir("rm");
        let path = dir.join("mcp.json");
        let original = r#"{"first":"kept","mcpServers":{"other":{"command":"x"},"bite":{"command":"bite","args":["mcp"]}}}"#;
        std::fs::write(&path, original).unwrap();
        let spec = spec_in(&dir, "mcp.json", ConfigKind::McpServers);
        assert!(spec.remove().unwrap());
        let v: Value = serde_json::from_str(&read(&path)).unwrap();
        assert_eq!(v["first"], "kept");
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert!(v["mcpServers"].get(SERVER_KEY).is_none());
        assert_eq!(read(&backup_path(&path)), original);
        // removing again is a no-op
        assert!(!spec.remove().unwrap());
    }

    #[test]
    fn remove_unparseable_aborts_untouched() {
        let dir = tempdir("rmbad");
        let path = dir.join("mcp.json");
        let original = "broken {";
        std::fs::write(&path, original).unwrap();
        let spec = spec_in(&dir, "mcp.json", ConfigKind::McpServers);
        let err = spec.remove().unwrap_err();
        assert!(err.contains("nothing was written"), "{err}");
        assert_eq!(read(&path), original);
    }

    #[test]
    fn remove_jsonc_preserves_comments() {
        let dir = tempdir("rmjsonc");
        let path = dir.join("opencode.jsonc");
        let original = "{\n  // my instructions\n  \"instructions\": [\"MEMORY.md\"],\n  \"mcp\": {\"bite\": {\"type\": \"local\", \"command\": [\"bite\", \"mcp\"]}}\n}\n";
        std::fs::write(&path, original).unwrap();
        let spec = spec_in(&dir, "opencode.jsonc", ConfigKind::OpenCode);
        assert!(spec.remove().unwrap());
        let out = read(&path);
        assert!(out.contains("// my instructions"));
        assert!(out.contains("MEMORY.md"));
        let v = jsonc_value(&out);
        assert!(v.get("mcp").and_then(|m| m.get(SERVER_KEY)).is_none());
        assert_eq!(read(&backup_path(&path)), original);
    }

    #[test]
    fn remove_toml_and_absent_files() {
        let dir = tempdir("rmtoml");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "model = \"gpt-5\"\n\n[mcp_servers.bite]\ncommand = \"bite\"\nargs = [\"mcp\"]\n",
        )
        .unwrap();
        let spec = spec_in(&dir, "config.toml", ConfigKind::CodexToml);
        assert!(spec.remove().unwrap());
        let doc: toml_edit::DocumentMut = read(&path).parse().unwrap();
        assert_eq!(doc["model"].as_str(), Some("gpt-5"));
        assert!(doc
            .get("mcp_servers")
            .and_then(|m| m.get(SERVER_KEY))
            .is_none());

        // absent file: no error, no removal
        let absent = spec_in(&dir, "missing.json", ConfigKind::McpServers);
        assert!(!absent.remove().unwrap());
    }

    // ── atomic writes ────────────────────────────────────────────────────

    #[test]
    fn atomic_write_completes_and_leaves_no_temp_files() {
        let dir = tempdir("atomic");
        let path = dir.join("config/config.json");
        let body = "{\"big\":\"value\"}".repeat(1000);
        atomic_write(&path, &body).unwrap();
        assert_eq!(read(&path), body);
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".bite-tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn atomic_write_failure_cleans_temp_and_keeps_target() {
        let dir = tempdir("atomicfail");
        let path = dir.join("config.json");
        // a directory at the target path makes the final rename fail
        std::fs::create_dir(&path).unwrap();
        let err = atomic_write(&path, "new contents").unwrap_err();
        assert!(!err.is_empty());
        assert!(path.is_dir(), "botched target untouched");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".bite-tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp cleaned up on failure");
    }

    #[test]
    fn stale_temp_file_from_a_killed_run_is_ignored() {
        // Simulate an interrupted write: a partial temp file exists next to
        // the target (what a kill -9 mid-`fs::write` leaves behind — except
        // with atomic_write the TARGET below is still the complete original).
        let dir = tempdir("stale");
        let path = dir.join("opencode.json");
        let original = "{\n  \"mine\": true\n}\n";
        std::fs::write(&path, original).unwrap();
        std::fs::write(
            dir.join(".opencode.json.bite-tmp-999999-999999"),
            "{\"partial",
        )
        .unwrap();

        let spec = spec_in(&dir, "opencode.json", ConfigKind::OpenCode);
        spec.add().unwrap();
        let v = jsonc_value(&read(&path));
        assert_eq!(v["mine"], true, "target was the complete old file");
        assert_eq!(v["mcp"][SERVER_KEY]["type"], "local");
        // the stale temp never became the config and was not clobbered into one
        assert_eq!(
            read(&dir.join(".opencode.json.bite-tmp-999999-999999")),
            "{\"partial"
        );
    }

    // ── sandboxed integration matrix (drives add()/remove() directly) ────

    /// The current documented entry shape for each kind.
    fn assert_bite_entry(kind: ConfigKind, out: &str) {
        match kind {
            ConfigKind::CodexToml => {
                let doc: toml_edit::DocumentMut = out.parse().expect("valid toml");
                assert_eq!(
                    doc["mcp_servers"][SERVER_KEY]["command"].as_str(),
                    Some("bite")
                );
                let args: Vec<String> = doc["mcp_servers"][SERVER_KEY]["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                assert_eq!(args, vec!["mcp"]);
            }
            _ => {
                let v = jsonc_value(out);
                let (key, want) = match kind {
                    ConfigKind::Servers => (
                        "servers",
                        json!({"type": "stdio", "command": "bite", "args": ["mcp"]}),
                    ),
                    ConfigKind::OpenCode => {
                        ("mcp", json!({"type": "local", "command": ["bite", "mcp"]}))
                    }
                    _ => ("mcpServers", json!({"command": "bite", "args": ["mcp"]})),
                };
                assert_eq!(v[key][SERVER_KEY], want, "documented shape for {kind:?}");
            }
        }
    }

    /// The user's own data (and comments) survived an edit.
    fn assert_user_data(kind: ConfigKind, out: &str, commented: bool) {
        match kind {
            ConfigKind::CodexToml => {
                let doc: toml_edit::DocumentMut = out.parse().expect("valid toml");
                assert_eq!(doc["other"].as_integer(), Some(1));
                if commented {
                    assert!(out.contains("# codex cfg"), "comment lost:\n{out}");
                }
            }
            ConfigKind::Servers => {
                let v = jsonc_value(out);
                assert_eq!(v["servers"]["x"]["command"], "x");
                if commented {
                    assert!(out.contains("// vscode cfg"), "comment lost:\n{out}");
                }
            }
            ConfigKind::McpServers => {
                let v = jsonc_value(out);
                assert_eq!(v["other"], 1);
                if commented {
                    assert!(out.contains("// claude cfg"), "comment lost:\n{out}");
                }
            }
            ConfigKind::OpenCode => {
                let v = jsonc_value(out);
                if commented {
                    assert!(out.contains("// my instructions"), "comment lost:\n{out}");
                    assert_eq!(v["instructions"], json!(["MEMORY.md"]));
                } else {
                    assert_eq!(v["other"], 1);
                }
            }
        }
    }

    /// Every kind × every starting state, against fixture files in a fake
    /// sandbox: {valid, commented, unparseable, empty, absent} per client,
    /// no real HOME touched.
    #[test]
    fn setup_matrix_every_kind_every_scenario() {
        struct Case {
            kind: ConfigKind,
            name: &'static str,
            valid: &'static str,
            commented: &'static str,
            broken: &'static str,
        }
        let cases = [
            Case {
                kind: ConfigKind::McpServers,
                name: "claude.json",
                valid: r#"{"other":1}"#,
                commented: "{\n  // claude cfg\n  \"other\": 1\n}\n",
                broken: "{ nope ",
            },
            Case {
                kind: ConfigKind::Servers,
                name: "mcp.json",
                valid: r#"{"servers":{"x":{"command":"x"}}}"#,
                commented: "{\n  // vscode cfg\n  \"servers\": {}\n}\n",
                broken: "{ nope ",
            },
            Case {
                kind: ConfigKind::OpenCode,
                name: "opencode.jsonc",
                valid: "{\n  \"other\": 1\n}\n",
                commented: "{\n  // my instructions\n  \"instructions\": [\"MEMORY.md\"]\n}\n",
                broken: "{ nope ",
            },
            Case {
                kind: ConfigKind::CodexToml,
                name: "config.toml",
                valid: "other = 1\n",
                commented: "# codex cfg\nother = 1\n", // TOML comments are native
                broken: "not [valid\n",
            },
        ];

        for (i, case) in cases.iter().enumerate() {
            let dir = tempdir(&format!("matrix{i}"));
            let spec_for = |name: String| spec_in(&dir, &name, case.kind);

            // — 1: existing valid config with unknown keys → merged, backed up —
            let p = dir.join(format!("v-{}", case.name));
            std::fs::write(&p, case.valid).unwrap();
            spec_for(format!("v-{}", case.name)).add().unwrap();
            let out = read(&p);
            assert_user_data(case.kind, &out, false);
            assert_bite_entry(case.kind, &out);
            assert_eq!(read(&backup_path(&p)), case.valid, "backup of original");

            // — 2: commented config — native for TOML/OpenCode (edited),
            //    refused untouched for strict-JSON clients —
            let p = dir.join(format!("c-{}", case.name));
            std::fs::write(&p, case.commented).unwrap();
            match case.kind {
                ConfigKind::CodexToml | ConfigKind::OpenCode => {
                    spec_for(format!("c-{}", case.name)).add().unwrap();
                    let out = read(&p);
                    assert_user_data(case.kind, &out, true);
                    assert_bite_entry(case.kind, &out);
                }
                ConfigKind::McpServers | ConfigKind::Servers => {
                    let err = spec_for(format!("c-{}", case.name)).add().unwrap_err();
                    assert!(err.contains("nothing was written"), "{err}");
                    assert_eq!(read(&p), case.commented, "refused file untouched");
                }
            }

            // — 3: unparseable → refuse, byte-identical —
            let p = dir.join(format!("b-{}", case.name));
            std::fs::write(&p, case.broken).unwrap();
            let err = spec_for(format!("b-{}", case.name)).add().unwrap_err();
            assert!(err.contains("nothing was written"), "{err}");
            assert!(err.contains(case.name), "names the file: {err}");
            assert_eq!(read(&p), case.broken);

            // — 4: empty file → fresh config with just the entry —
            let p = dir.join(format!("e-{}", case.name));
            std::fs::write(&p, "").unwrap();
            spec_for(format!("e-{}", case.name)).add().unwrap();
            let out = read(&p);
            assert_bite_entry(case.kind, &out);
            assert!(!backup_path(&p).exists(), "nothing to back up");

            // — 5: absent file → fresh config with just the entry —
            let p = dir.join(format!("a-{}", case.name));
            spec_for(format!("a-{}", case.name)).add().unwrap();
            let out = read(&p);
            assert_bite_entry(case.kind, &out);
            assert!(!backup_path(&p).exists(), "nothing to back up");
            match case.kind {
                ConfigKind::CodexToml => {
                    let doc: toml_edit::DocumentMut = out.parse().unwrap();
                    let keys: Vec<String> = doc.iter().map(|(k, _)| k.to_string()).collect();
                    assert_eq!(keys, vec!["mcp_servers"], "only our table");
                }
                _ => {
                    let v = jsonc_value(&out);
                    assert_eq!(v.as_object().unwrap().len(), 1, "only our key");
                }
            }

            // — round-trip: remove() undoes the entry, keeps user data —
            assert!(
                spec_for(format!("v-{}", case.name)).remove().unwrap(),
                "removal happened for {}",
                case.name
            );
            let out = read(&dir.join(format!("v-{}", case.name)));
            assert!(!out.contains(SERVER_KEY), "entry gone:\n{out}");
            assert_user_data(case.kind, &out, false);
        }
    }
}
