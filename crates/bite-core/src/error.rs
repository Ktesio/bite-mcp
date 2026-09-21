//! Error presentation: every failure carries an actionable fix — this is the
//! core UX contract of bite ("agent-friendly errors").

use bite_bridge::BridgeError;

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct BiteError(#[from] pub BridgeError);

impl BiteError {
    /// One-line human/agent message including the fix, if known.
    pub fn render(&self) -> String {
        let e = &self.0;
        match &e.fix {
            Some(fix) => format!("{} ({code}) — fix: {fix}", e.message, code = e.code),
            None => format!("{} ({code})", e.message, code = e.code),
        }
    }

    pub fn code(&self) -> &str {
        &self.0.code
    }
}

/// Convenience constructor for CLI-level (pre-bridge) errors.
pub fn cli_error(message: impl Into<String>) -> BiteError {
    BiteError(BridgeError::new("cli_error", message))
}
