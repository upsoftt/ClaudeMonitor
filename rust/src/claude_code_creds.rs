//! Read/write `~/.claude/.credentials.json` — the OAuth tokens Claude Code
//! uses for `claude` CLI sessions.
//!
//! When the user switches the active claude.ai account in our overlay we mirror
//! their cc_tokens into `.credentials.json` so the CLI uses the same identity.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::types::CcTokens;

/// File schema. Claude Code stores tokens under a `claudeAiOauth` key.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CredentialsFile {
    #[serde(rename = "claudeAiOauth", default)]
    pub claude_ai_oauth: Option<CcTokens>,

    /// Preserve any other top-level keys verbatim.
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// Read `~/.claude/.credentials.json` and return the OAuth tokens, if any.
pub fn read() -> Result<Option<CcTokens>> {
    let path = match crate::paths::claude_code_creds() {
        Some(p) => p,
        None => return Ok(None),
    };
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let file: CredentialsFile = serde_json::from_str(&raw).unwrap_or_default();
    Ok(file.claude_ai_oauth)
}

/// Atomically write the given OAuth tokens to `~/.claude/.credentials.json`,
/// preserving any unknown top-level keys.
pub fn write(tokens: &CcTokens) -> Result<()> {
    let path = crate::paths::claude_code_creds()
        .context("home dir not available — cannot resolve ~/.claude/.credentials.json")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut file: CredentialsFile = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => CredentialsFile::default(),
    };
    file.claude_ai_oauth = Some(tokens.clone());
    let body = serde_json::to_string_pretty(&file)?;
    crate::account_manager::atomic_write(&path, body.as_bytes())?;
    Ok(())
}

/// Apply the active account's tokens (if any) to `.credentials.json`.
/// Called from the account-switch handler.
pub fn sync_active_account(am: &crate::account_manager::AccountManager) -> Result<()> {
    let aid = match am.active_id() {
        Some(a) => a,
        None => return Ok(()),
    };
    if let Some(tok) = am.cc_tokens(&aid) {
        write(&tok)?;
        tracing::info!(account = %aid, "wrote claude-code tokens for active account");
    }
    Ok(())
}

/// On startup: if `accounts_meta.json` doesn't have cc_tokens for the active
/// account but `.credentials.json` does — backfill them.
pub fn backfill_from_disk(am: &crate::account_manager::AccountManager) -> Result<()> {
    let aid = match am.active_id() {
        Some(a) => a,
        None => return Ok(()),
    };
    if am.cc_tokens(&aid).is_some() {
        return Ok(());
    }
    if let Some(disk) = read()? {
        am.update_cc_tokens(&aid, disk)?;
        tracing::info!(account = %aid, "backfilled claude-code tokens from .credentials.json");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_credentials_file() {
        let tok = CcTokens {
            access_token: "sk-ant-oat01-X".into(),
            refresh_token: "sk-ant-ort01-Y".into(),
            expires_at: 12345,
            scopes: vec!["user:profile".into()],
            subscription_type: "max".into(),
            rate_limit_tier: "default_claude_max_20x".into(),
        };
        let mut file = CredentialsFile::default();
        file.claude_ai_oauth = Some(tok.clone());
        // Some Claude Code installs may have unrelated keys we must preserve.
        file.extras.insert("misc".into(), serde_json::json!({"foo": "bar"}));
        let s = serde_json::to_string(&file).unwrap();
        let parsed: CredentialsFile = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.claude_ai_oauth.as_ref().unwrap().expires_at, 12345);
        assert!(parsed.extras.contains_key("misc"));
    }
}
