//! Loading the Anthropic API key.
//!
//! The key is read from 1Password at startup with the 1Password CLI
//! (`op read "op://<vault>/<item>/credential"`) and kept only in memory. It
//! never goes into config files, logs or the repository. The secret
//! reference itself (not the key) comes from the `CHESS_TUTOR_OP_REF`
//! environment variable.

use std::process::Command;

/// Environment variable holding the 1Password secret reference.
pub const OP_REF_VAR: &str = "CHESS_TUTOR_OP_REF";

/// An API key that never prints itself.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> ApiKey {
        ApiKey(key.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error(
        "no API key source: set {OP_REF_VAR} to a 1Password reference such as op://Private/Anthropic/credential"
    )]
    NotConfigured,
    #[error("could not run the 1Password CLI (`op`): {0}")]
    OpMissing(std::io::Error),
    #[error("1Password CLI failed: {0}")]
    OpFailed(String),
    #[error("1Password returned an empty secret")]
    Empty,
}

/// Where the key came from, for the status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    OnePassword,
    Environment,
}

/// Reads the key from 1Password using the reference in `CHESS_TUTOR_OP_REF`.
///
/// For development without 1Password, `ANTHROPIC_API_KEY` is accepted as a
/// fallback when no reference is configured.
pub fn load_api_key() -> Result<(ApiKey, KeySource), SecretError> {
    if let Ok(reference) = std::env::var(OP_REF_VAR)
        && !reference.trim().is_empty()
    {
        return read_from_1password(reference.trim()).map(|k| (k, KeySource::OnePassword));
    }
    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.trim().is_empty() => Ok((ApiKey::new(k.trim()), KeySource::Environment)),
        _ => Err(SecretError::NotConfigured),
    }
}

/// Runs `op read <reference>` and returns the secret.
pub fn read_from_1password(reference: &str) -> Result<ApiKey, SecretError> {
    let output = Command::new("op")
        .args(["read", "--no-newline", reference])
        .output()
        .map_err(SecretError::OpMissing)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SecretError::OpFailed(stderr));
    }
    let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if key.is_empty() {
        return Err(SecretError::Empty);
    }
    Ok(ApiKey(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_redacted_in_debug_output() {
        let key = ApiKey::new("sk-ant-secret");
        assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
        assert_eq!(key.expose(), "sk-ant-secret");
    }
}
