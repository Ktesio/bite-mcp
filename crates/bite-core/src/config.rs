//! User config: `~/Library/Application Support/bite/config.toml` on macOS.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Default calendar name for event creation when none passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_calendar: Option<String>,
    /// Default mail account for sends when none passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mail_account: Option<String>,
    /// Per-request bridge timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// GitHub repo for prebuilt helper downloads (`owner/repo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_repo: Option<String>,
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
    pub fn load() -> Self {
        std::fs::read_to_string(config_path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        crate::fsops::write_private(&path, toml::to_string_pretty(self).unwrap_or_default().as_bytes())
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
}
