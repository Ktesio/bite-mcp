//! Docs↔registry conformance: every tool the registry exposes must be
//! documented in docs/protocol.md — helper-facing methods in the Method
//! table (group + short-name shorthand), index-plane tools by MCP name in
//! their own Part-2 sections. protocol_conformance.rs pins the registry
//! against the Swift dispatcher; this file pins it against the prose, so a
//! registry addition that skips the docs reds CI instead of drifting (the
//! historical 38-vs-45 as-built gap was caught by eye).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

fn protocol_md() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/protocol.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// One entry of the Method table: a full bridge method (`calendar.event_create`)
/// plus the identifiers named in its `{...}` param shorthand, if any.
struct TableEntry {
    method: String,
    params: Vec<String>,
}

/// Parse the `## Method table` section into entries.
///
/// The table is grouped prose wrapped across lines:
/// `calendar: \`list_calendars\` · \`event_get {id}\` · …` with free
/// parenthetical annotation between and around entries. Three rules
/// survive the prose:
/// - a line starting `label:` (lowercase identifier, colon, backtick)
///   opens a group — labels never share a line with another group;
/// - a backticked token is an ENTRY only when it carries a `{...}`
///   param shorthand itself or is immediately followed by the `·`
///   separator / end of line — prose backticks (`would_delete`,
///   `truncated`, `chat.db`) are always followed by words;
/// - the entry verb is the first word of the token and must look like
///   an identifier (`probe:true` is annotation, not a verb).
///
/// `sys.*` entries carry the full dotted name; every other group uses
/// short names qualified by the group label.
fn method_table(md: &str) -> Vec<TableEntry> {
    let section = md
        .split("\n## ")
        .find(|s| s.starts_with("Method table"))
        .expect("docs/protocol.md must keep a `## Method table` section");
    let mut out = Vec::new();
    let mut group = String::new();
    for line in section.lines() {
        let trimmed = line.trim_start();
        if let Some(colon) = trimmed.find(':') {
            let label = &trimmed[..colon];
            if !label.is_empty()
                && label.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                && trimmed[colon + 1..].trim_start().starts_with('`')
            {
                group = label.to_string();
            }
        }
        let mut rest = line;
        while let Some(t1) = rest.find('`') {
            let after = &rest[t1 + 1..];
            let Some(t2) = after.find('`') else { break };
            let token = &after[..t2];
            rest = &after[t2 + 1..];
            let Some(verb) = token.split_whitespace().next() else {
                continue;
            };
            let identifier = !verb.is_empty()
                && verb
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
                && verb.chars().any(|c| c.is_ascii_alphabetic());
            let followed = rest.trim_start();
            let entry = token.contains('{') || followed.starts_with('·') || followed.is_empty();
            if !identifier || group.is_empty() || !entry {
                continue;
            }
            let method = if verb.contains('.') {
                verb.to_string()
            } else {
                format!("{group}.{verb}")
            };
            // `{to[], cc?, subject}` → the identifiers inside, ignoring
            // ?, [], =default and a/b alternatives.
            let params: Vec<String> = match (token.find('{'), token.rfind('}')) {
                (Some(b), Some(e)) if b < e => token[b + 1..e]
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .filter(|w| !w.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => Vec::new(),
            };
            out.push(TableEntry { method, params });
        }
    }
    out
}

#[test]
fn helper_methods_are_documented_in_the_method_table() {
    let table: HashSet<String> = method_table(&protocol_md())
        .iter()
        .map(|e| e.method.clone())
        .collect();
    let mut missing = Vec::new();
    for tool in bite_core::tools() {
        // index.* never crosses the helper bridge — documented in its own
        // sections (see index_plane_tools_are_documented).
        if tool.method.starts_with("index.") {
            continue;
        }
        if !table.contains(tool.method) {
            missing.push(tool.method);
        }
    }
    assert!(
        missing.is_empty(),
        "registry methods missing from the docs/protocol.md Method table: \
         {missing:?} — add each to its namespace group line"
    );
}

#[test]
fn method_table_entries_exist_in_the_registry() {
    let mut strays = Vec::new();
    for entry in method_table(&protocol_md()) {
        // sys.* is bridge infrastructure (ping/version/probe) with no
        // registry tool behind it.
        if entry.method.starts_with("sys.") {
            continue;
        }
        if bite_core::tools().iter().all(|t| t.method != entry.method) {
            strays.push(entry.method);
        }
    }
    assert!(
        strays.is_empty(),
        "docs/protocol.md Method table lists methods the registry no longer \
         has: {strays:?} — remove or update the stale entries"
    );
}

#[test]
fn required_params_appear_in_the_table_shorthand() {
    let table: HashMap<String, Vec<String>> = method_table(&protocol_md())
        .into_iter()
        .map(|e| (e.method, e.params))
        .collect();
    for tool in bite_core::tools() {
        if tool.method.starts_with("index.") {
            continue;
        }
        let Some(shorthand) = table.get(tool.method) else {
            continue; // presence itself is pinned by the tests above
        };
        for p in tool.params.iter().filter(|p| p.required) {
            assert!(
                shorthand.iter().any(|w| w == p.name),
                "{}'s Method table shorthand omits required param `{}` — \
                 update docs/protocol.md",
                tool.method,
                p.name
            );
        }
    }
}

#[test]
fn index_plane_tools_are_documented() {
    let md = protocol_md();
    // Backtick tokens outside fenced code blocks (the ```json examples
    // quote method names that must not satisfy this fence).
    let mut tokens = Vec::new();
    let mut fenced = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let mut rest = line;
        while let Some(t1) = rest.find('`') {
            let after = &rest[t1 + 1..];
            match after.find('`') {
                Some(t2) => {
                    tokens.push(after[..t2].to_string());
                    rest = &after[t2 + 1..];
                }
                None => break,
            }
        }
    }
    for tool in bite_core::tools() {
        if !tool.method.starts_with("index.") {
            continue;
        }
        // Documented by MCP name in the index sections; a `prefix*`
        // wildcard token (the mail_bulk_* trio) also satisfies it.
        // index_wipe is CLI-only by design and intentionally unbacked.
        let documented = tokens.iter().any(|tok| {
            tok == tool.name
                || tok.ends_with('*') && tool.name.starts_with(tok.trim_end_matches('*'))
        });
        assert!(
            documented,
            "index-plane tool `{}` (method {}) is not named in docs/protocol.md \
             — index.* tools are documented by MCP name in the index sections",
            tool.name, tool.method
        );
    }
}
