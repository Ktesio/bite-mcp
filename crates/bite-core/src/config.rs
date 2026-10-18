//! User config: `~/Library/Application Support/bite/config.toml` on macOS.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Default calendar name for event creation when none passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_calendar: Option<String>,
    /// Default mail account for sends when none passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mail_account: Option<String>,
    /// Per-request bridge timeout in seconds (env override: `BITE_TIMEOUT_SECS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// GitHub repo for prebuilt helper downloads (`owner/repo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_repo: Option<String>,
    /// Entity-index auto-refresh threshold in hours (default 24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_refresh_hours: Option<u64>,
}

/// Process-lifetime config cache backing [`Config::cached`]. Every MCP tool
/// call used to re-read config.toml from disk; within one long-lived `bite
/// mcp` server the file cannot change meaningfully: the only writer is the
/// `bite config set` CLI verb, which runs in its own short-lived process.
static CACHED_CONFIG: OnceLock<Config> = OnceLock::new();

/// Testable core of [`Config::cached`]: fill `cell` from `load` at most
/// once, then serve the cached copy. Separated so tests can drive it with
/// their own cell and a counting loader instead of the real config file.
fn cached_in(cell: &OnceLock<Config>, load: fn() -> Config) -> Config {
    cell.get_or_init(load).clone()
}

pub fn data_dir() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("bite")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn helper_install_path() -> PathBuf {
    data_dir().join("bin").join("bite-helper")
}

impl Config {
    /// Load from disk once per process, then serve the cached copy.
    ///
    /// The hot path (every MCP tool call, every CLI verb) used to re-read
    /// `config.toml` from disk; within a long-lived `bite mcp` process the
    /// file cannot change meaningfully — the only writer is the `bite
    /// config set` CLI verb, which is CLI-only and always runs in its own
    /// process that re-exits right after saving. Callers that need the
    /// freshest possible view (or that write the file) keep using
    /// [`Config::load`]; call sites that run per tool call use this.
    pub fn cached() -> Self {
        cached_in(&CACHED_CONFIG, Config::load)
    }

    pub fn load() -> Self {
        let mut cfg: Self = std::fs::read_to_string(config_path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        // `BITE_TIMEOUT_SECS` env override: wins over the config file. Tests
        // use it to bound per-call budgets to something the suite can wait
        // for; it also serves as a one-off ops lever without editing the file.
        if let Some(secs) = std::env::var("BITE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            cfg.timeout_secs = Some(secs);
        }
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        crate::fsops::write_private(
            &path,
            toml::to_string_pretty(self).unwrap_or_default().as_bytes(),
        )
    }

    pub fn timeout(&self) -> std::time::Duration {
        // generous ceiling: slow Apple Event queries on large mailboxes can
        // legitimately take a while when Mail is busy indexing
        std::time::Duration::from_secs(self.timeout_secs.unwrap_or(300))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let c = Config {
            default_calendar: Some("Home".into()),
            ..Default::default()
        };
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.default_calendar.as_deref(), Some("Home"));
    }

    #[test]
    fn cached_initializes_exactly_once() {
        // own cell + counting loader: proves the cache semantics without
        // touching the developer's real config file. A regression of
        // `cached()` to a plain per-call load would run the loader twice.
        static CELL: OnceLock<Config> = OnceLock::new();
        static LOADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn counting_load() -> Config {
            LOADS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Config {
                timeout_secs: Some(42),
                ..Default::default()
            }
        }
        let a = cached_in(&CELL, counting_load);
        let b = cached_in(&CELL, counting_load);
        assert_eq!(a.timeout_secs, Some(42));
        assert_eq!(b.timeout_secs, Some(42), "later calls get the cached value");
        assert_eq!(
            LOADS.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the loader must run exactly once"
        );
    }
}
