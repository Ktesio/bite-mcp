//! Docs↔Config conformance: every key the `Config` struct accepts must have
//! a row in docs/configuration.md's Keys table, and the table must not list
//! keys that no longer exist — the env-hooks table is pinned the same way.
//! docs_conformance.rs pins protocol.md against the tool registry; this
//! file pins the configuration page against the `Config` serializer, so a
//! config key that ships without documentation (or a documented key that
//! outlives its field) reds CI instead of drifting.

use std::path::PathBuf;

fn configuration_md() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/configuration.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The `## <title>` section body, or panic naming the missing section —
/// a renamed/deleted section must fail loudly, not read as "no rows".
fn section<'a>(md: &'a str, title: &str) -> &'a str {
    let needle = format!("\n## {title}\n");
    let Some(start) = md.find(&needle) else {
        panic!("docs/configuration.md must keep a `## {title}` section")
    };
    let body_start = start + needle.len();
    let end = md[body_start..]
        .find("\n## ")
        .map(|i| body_start + i)
        .unwrap_or(md.len());
    &md[body_start..end]
}

/// First backticked token of each table row (`| \`key\` | … |`) in the
/// section. Header and separator rows carry no backticks and are skipped;
/// later backticks in a row (units, cross-references) are ignored.
fn table_keys(section: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in section.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('|') {
            continue;
        }
        let Some(t1) = trimmed.find('`') else {
            continue;
        };
        let after = &trimmed[t1 + 1..];
        let Some(t2) = after.find('`') else { continue };
        out.push(after[..t2].to_string());
    }
    out
}

/// serde field names ARE the TOML keys: serialize a fully-populated
/// `Config` and parse it back, so the fence tracks the wire format rather
/// than a hand-copied field list.
fn config_toml_keys() -> Vec<String> {
    let cfg = bite_core::config::Config {
        default_calendar: Some("cal".into()),
        default_mail_account: Some("acct".into()),
        timeout_secs: Some(300),
        release_repo: Some("owner/repo".into()),
        auto_refresh_hours: Some(24),
    };
    let text = toml::to_string_pretty(&cfg).expect("serialize config");
    let value: toml::Value = toml::from_str(&text).expect("parse serialized config");
    value
        .as_table()
        .expect("config serializes to a top-level table")
        .keys()
        .cloned()
        .collect()
}

#[test]
fn every_config_key_is_documented() {
    let documented: Vec<String> = table_keys(section(&configuration_md(), "Keys"));
    let mut missing: Vec<String> = config_toml_keys()
        .into_iter()
        .filter(|k| !documented.contains(k))
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "config keys missing a row in docs/configuration.md's Keys table: {missing:?} — \
         a key that ships undocumented breaks this fence by design"
    );
}

#[test]
fn every_documented_key_exists_in_config() {
    let keys = config_toml_keys();
    let mut strays: Vec<String> = table_keys(section(&configuration_md(), "Keys"))
        .into_iter()
        .filter(|k| !keys.contains(k))
        .collect();
    strays.sort();
    assert!(
        strays.is_empty(),
        "docs/configuration.md Keys table lists keys the Config struct no longer has: \
         {strays:?} — remove or update the stale rows"
    );
}

#[test]
fn env_hooks_table_is_exact() {
    let documented = table_keys(section(&configuration_md(), "Environment hooks"));
    let expected = ["BITE_TIMEOUT_SECS", "BITE_HELPER_BIN"];
    for var in expected {
        assert!(
            documented.iter().any(|v| v == var),
            "`{var}` must have a row in docs/configuration.md's Environment hooks table"
        );
    }
    let strays: Vec<&String> = documented
        .iter()
        .filter(|v| !expected.contains(&v.as_str()))
        .collect();
    assert!(
        strays.is_empty(),
        "Environment hooks table rows that are not user-facing env hooks: {strays:?} — \
         build-time variables belong in prose, not the table"
    );
}

#[test]
fn precedence_section_names_the_env_override() {
    let md = configuration_md();
    let precedence = section(&md, "Precedence");
    assert!(
        precedence.contains("BITE_TIMEOUT_SECS"),
        "the Precedence section must show where `BITE_TIMEOUT_SECS` sits in the layer order"
    );
    assert!(
        precedence.contains("300"),
        "the Precedence section must name the built-in `timeout_secs` default"
    );
}
