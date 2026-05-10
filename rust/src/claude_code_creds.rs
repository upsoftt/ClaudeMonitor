//! Read/write `~/.claude/.credentials.json` — the OAuth tokens Claude Code
//! uses for `claude` CLI sessions.
//!
//! When the user switches the active claude.ai account in our overlay we mirror
//! their cc_tokens into `.credentials.json` so the CLI uses the same identity.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::account_manager::AccountManager;
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
///
/// We deliberately overwrite `expiresAt` with `1` so that the running `claude`
/// CLI treats the token as expired on its next request and exchanges the
/// refreshToken — which now belongs to the *new* account — for a fresh access
/// token. The CLI then writes those fresh tokens back to `.credentials.json`,
/// and our `watch_loop` captures them into `accounts_meta.json` so we don't
/// drift over time. Net effect: switching accounts in the UI surfaces in any
/// new (and most live) `claude` sessions without requiring `/login`.
pub fn sync_active_account(am: &AccountManager) -> Result<()> {
    let aid = match am.active_id() {
        Some(a) => a,
        None => return Ok(()),
    };
    if let Some(mut tok) = am.cc_tokens(&aid) {
        tok.expires_at = 1;
        write(&tok)?;
        tracing::info!(account = %aid, "wrote claude-code tokens (force-refresh) for active account");
    }
    Ok(())
}

/// Periodically poll `.credentials.json` and capture any fresh tokens written
/// by the CLI (e.g. after its automatic refresh, or after the user does
/// `/login` directly). Updates the active account's `cc_tokens` in
/// `accounts_meta.json` so our stored refreshToken stays valid for future
/// switches.
pub async fn watch_loop(am: Arc<AccountManager>, mut shutdown_rx: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen_token = String::new();

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let disk = match read() {
                    Ok(Some(t)) => t,
                    _ => continue,
                };
                if disk.access_token.is_empty() || disk.access_token == last_seen_token {
                    continue;
                }
                last_seen_token = disk.access_token.clone();

                let active = match am.active_id() {
                    Some(a) => a,
                    None => continue,
                };
                let stored = am.cc_tokens(&active);
                let differs = stored
                    .as_ref()
                    .map_or(true, |s| s.access_token != disk.access_token);
                // Skip the special force-refresh marker we just wrote — the
                // CLI hasn't refreshed yet, so the disk token is still ours.
                if differs && disk.expires_at != 1 {
                    if let Err(e) = am.update_cc_tokens(&active, disk) {
                        tracing::warn!(error = %e, "update_cc_tokens failed");
                    } else {
                        tracing::info!(account = %active, "captured refreshed CC tokens from .credentials.json");
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { break; }
            }
        }
    }
}

/// On startup: if `accounts_meta.json` doesn't have cc_tokens for the active
/// account but `.credentials.json` does — backfill them.
pub fn backfill_from_disk(am: &AccountManager) -> Result<()> {
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
