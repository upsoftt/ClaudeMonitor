//! Multi-account storage. 1:1 port of the Python `AccountManager`
//! (usage_monitor.py:539-799).
//!
//! Files:
//!   accounts/<aid>.json     — Playwright storage_state (cookies + origins)
//!   accounts_meta.json      — list, active id, removed_orgs blacklist, cc_tokens
//!   claude_auth.json        — legacy single-account auth (migrated on first run)
//!
//! Wire compatibility: the on-disk format must round-trip with the Python
//! version (operators may run both side-by-side during migration).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::Utc;
use md5::{Digest, Md5};
use parking_lot::Mutex;
use serde_json::Value;

use crate::paths;
use crate::types::{Account, AccountsMeta, CcTokens, Cookie, StorageState};

/// Atomic write: tmp file in the same dir + rename.
/// On Windows rename is non-atomic across drives but is on the same drive.
pub fn atomic_write(target: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("create tempfile in {}", dir.display()))?;
    use std::io::Write;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.persist(target)
        .map_err(|e| anyhow::anyhow!("persist {} failed: {}", target.display(), e.error))?;
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Result<T> {
    let s = std::fs::read_to_string(p)
        .with_context(|| format!("read {}", p.display()))?;
    Ok(serde_json::from_str(&s)
        .with_context(|| format!("parse {}", p.display()))?)
}

fn write_json<T: serde::Serialize>(p: &Path, v: &T) -> Result<()> {
    let body = serde_json::to_string_pretty(v)?;
    atomic_write(p, body.as_bytes())
}

/// Result of `save_cookies`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Account did not exist before — first time we see this session.
    Created,
    /// Existing account, refreshed cookies.
    Refreshed,
    /// Push was rejected (blacklisted org, no sessionKey, etc).
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveResult {
    pub account_id: Option<String>,
    pub outcome: SaveOutcome,
}

pub struct AccountManager {
    app_dir: PathBuf,
    accounts_dir: PathBuf,
    meta_file: PathBuf,
    legacy_file: PathBuf,
    /// Time-window bypass for the removed-orgs blacklist (`+ Add account` button).
    /// Set as a Unix-millis deadline; `0` means no bypass active.
    /// Lazy-spend: only consumed once a *new* account is actually created from
    /// a blacklisted org.
    bypass_until_ms: Mutex<u64>,
}

impl AccountManager {
    pub fn new(app_dir: &Path) -> Result<Self> {
        let accounts_dir = paths::accounts_dir(app_dir);
        std::fs::create_dir_all(&accounts_dir)
            .with_context(|| format!("create {}", accounts_dir.display()))?;
        Ok(Self {
            app_dir: app_dir.to_path_buf(),
            accounts_dir,
            meta_file: paths::accounts_meta(app_dir),
            legacy_file: paths::legacy_auth(app_dir),
            bypass_until_ms: Mutex::new(0),
        })
    }

    fn load_meta(&self) -> AccountsMeta {
        if self.meta_file.exists() {
            read_json(&self.meta_file).unwrap_or_default()
        } else {
            AccountsMeta::default()
        }
    }

    fn save_meta(&self, meta: &AccountsMeta) -> Result<()> {
        write_json(&self.meta_file, meta)
    }

    pub fn account_file(&self, id: &str) -> PathBuf {
        self.accounts_dir.join(format!("{id}.json"))
    }

    pub fn active_id(&self) -> Option<String> {
        self.load_meta().active
    }

    /// Path to the active account's storage file, or the legacy single-account file.
    pub fn active_file(&self) -> Option<PathBuf> {
        let meta = self.load_meta();
        if let Some(aid) = &meta.active {
            let p = self.account_file(aid);
            if p.exists() {
                return Some(p);
            }
        }
        if self.legacy_file.exists() {
            return Some(self.legacy_file.clone());
        }
        None
    }

    /// Confirmed (non-pending) accounts.
    pub fn all(&self) -> Vec<Account> {
        self.load_meta()
            .accounts
            .into_iter()
            .filter(|a| !a.pending)
            .collect()
    }

    pub fn all_including_pending(&self) -> Vec<Account> {
        self.load_meta().accounts
    }

    /// Open the bypass window for the blacklist (called from `+ Add` button).
    pub fn unblock_next_save(&self, duration_secs: u64) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        *self.bypass_until_ms.lock() = now_ms + duration_secs * 1000;
    }

    fn bypass_active(&self) -> bool {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        now_ms < *self.bypass_until_ms.lock()
    }

    fn consume_bypass(&self) {
        *self.bypass_until_ms.lock() = 0;
    }

    /// Find existing confirmed account by `lastActiveOrg` cookie.
    /// Skips accounts without an email — those are still pending and would
    /// produce false matches if collide on the same org.
    fn find_by_stable_cookies(&self, new_cookies: &[Cookie]) -> Option<String> {
        let new_org = crate::types::last_active_org(new_cookies)?;
        for acc in self.all() {
            if acc.email.is_empty() {
                continue;
            }
            let f = self.account_file(&acc.id);
            if !f.exists() {
                continue;
            }
            let storage: StorageState = match read_json(&f) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Some(existing_org) = crate::types::last_active_org(&storage.cookies) {
                if existing_org == new_org {
                    return Some(acc.id);
                }
            }
        }
        None
    }

    /// Mark account as confirmed (no longer pending).
    pub fn confirm(&self, account_id: &str) -> Result<()> {
        let mut meta = self.load_meta();
        for acc in &mut meta.accounts {
            if acc.id == account_id {
                acc.pending = false;
            }
        }
        self.save_meta(&meta)
    }

    /// Persist a CookieBridge cookie list. Returns the affected account id +
    /// whether it's a new account.
    pub fn save_cookies(&self, cookies: Vec<Cookie>) -> Result<SaveResult> {
        let session_key = match crate::types::session_key(&cookies) {
            Some(s) => s.to_string(),
            None => return Ok(SaveResult { account_id: None, outcome: SaveOutcome::Rejected }),
        };
        let last_active_org = crate::types::last_active_org(&cookies)
            .map(String::from)
            .unwrap_or_default();

        let bypass = self.bypass_active();
        if !last_active_org.is_empty() && !bypass {
            let meta = self.load_meta();
            if meta.removed_orgs.iter().any(|o| o == &last_active_org) {
                return Ok(SaveResult { account_id: None, outcome: SaveOutcome::Rejected });
            }
        }

        let storage = StorageState { cookies: cookies.clone(), origins: vec![] };
        let storage_bytes = serde_json::to_vec_pretty(&storage)?;

        // Fast local dedup by stable cookies (no network).
        if let Some(stable_match) = self.find_by_stable_cookies(&cookies) {
            atomic_write(&self.account_file(&stable_match), &storage_bytes)?;
            self.confirm(&stable_match)?;
            // Auto-activate if no active account yet.
            let mut meta = self.load_meta();
            if meta.active.is_none() {
                meta.active = Some(stable_match.clone());
                self.save_meta(&meta)?;
            }
            return Ok(SaveResult {
                account_id: Some(stable_match),
                outcome: SaveOutcome::Refreshed,
            });
        }

        // New session — generate id from md5(sessionKey)[:10].
        let mut h = Md5::new();
        h.update(session_key.as_bytes());
        let aid = format!("acc_{}", &hex::encode(h.finalize())[..10]);

        let mut meta = self.load_meta();
        let exists = meta.accounts.iter().any(|a| a.id == aid);
        let outcome = if exists { SaveOutcome::Refreshed } else { SaveOutcome::Created };

        atomic_write(&self.account_file(&aid), &storage_bytes)?;

        if !exists {
            meta.accounts.push(Account {
                id: aid.clone(),
                email: String::new(),
                name: String::new(),
                plan: String::new(),
                uuid: String::new(),
                added_at: Utc::now().to_rfc3339(),
                pending: true,
                cc_tokens: None,
            });

            // New account just bypassed the blacklist (or wasn't in it):
            // consume the window and drop this org from removed_orgs.
            if bypass {
                self.consume_bypass();
                if !last_active_org.is_empty() {
                    meta.removed_orgs.retain(|o| o != &last_active_org);
                }
            }
        }

        if meta.active.is_none() {
            meta.active = Some(aid.clone());
        }
        self.save_meta(&meta)?;

        Ok(SaveResult { account_id: Some(aid), outcome })
    }

    pub fn switch_to(&self, account_id: &str) -> Result<()> {
        let mut meta = self.load_meta();
        meta.active = Some(account_id.to_string());
        self.save_meta(&meta)
    }

    pub fn update_info(
        &self,
        account_id: &str,
        email: Option<&str>,
        name: Option<&str>,
        plan: Option<&str>,
        uuid: Option<&str>,
    ) -> Result<()> {
        let mut meta = self.load_meta();
        for acc in &mut meta.accounts {
            if acc.id == account_id {
                if let Some(e) = email { if !e.is_empty() { acc.email = e.into(); } }
                if let Some(n) = name { if !n.is_empty() { acc.name = n.into(); } }
                if let Some(p) = plan { if !p.is_empty() { acc.plan = p.into(); } }
                if let Some(u) = uuid { if !u.is_empty() { acc.uuid = u.into(); } }
                break;
            }
        }
        self.save_meta(&meta)
    }

    pub fn find_by_uuid(&self, uuid: &str) -> Option<String> {
        if uuid.is_empty() {
            return None;
        }
        self.load_meta()
            .accounts
            .into_iter()
            .find(|a| a.uuid == uuid)
            .map(|a| a.id)
    }

    pub fn update_cc_tokens(&self, account_id: &str, tokens: CcTokens) -> Result<()> {
        let mut meta = self.load_meta();
        for acc in &mut meta.accounts {
            if acc.id == account_id {
                acc.cc_tokens = Some(tokens.clone());
                break;
            }
        }
        self.save_meta(&meta)
    }

    pub fn cc_tokens(&self, account_id: &str) -> Option<CcTokens> {
        self.load_meta()
            .accounts
            .into_iter()
            .find(|a| a.id == account_id)
            .and_then(|a| a.cc_tokens)
    }

    /// Remove account, capture its `lastActiveOrg` into `removed_orgs` so
    /// future bridge pushes are silently ignored.
    pub fn remove(&self, account_id: &str) -> Result<()> {
        let mut meta = self.load_meta();
        let f = self.account_file(account_id);
        let mut org = String::new();
        if let Ok(storage) = read_json::<StorageState>(&f) {
            if let Some(o) = crate::types::last_active_org(&storage.cookies) {
                org = o.to_string();
            }
        }

        meta.accounts.retain(|a| a.id != account_id);
        if meta.active.as_deref() == Some(account_id) {
            meta.active = meta.accounts.first().map(|a| a.id.clone());
        }
        if !org.is_empty() && !meta.removed_orgs.iter().any(|o| o == &org) {
            meta.removed_orgs.push(org);
        }
        self.save_meta(&meta)?;
        // Best-effort delete of cookie file.
        let _ = std::fs::remove_file(&f);
        Ok(())
    }

    /// Import legacy `claude_auth.json` as the first account if no accounts exist.
    pub fn migrate_legacy(&self) -> Result<()> {
        if !self.legacy_file.exists() || !self.all_including_pending().is_empty() {
            return Ok(());
        }
        let v: Value = match read_json::<Value>(&self.legacy_file) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        let cookies: Vec<Cookie> = serde_json::from_value(
            v.get("cookies").cloned().unwrap_or(Value::Array(vec![])),
        )
        .unwrap_or_default();
        if cookies.is_empty() {
            return Ok(());
        }
        let _ = self.save_cookies(cookies)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cookie(name: &str, value: &str) -> Cookie {
        Cookie {
            name: name.into(),
            value: value.into(),
            domain: ".claude.ai".into(),
            path: "/".into(),
            expires: -1.0,
            ..Default::default()
        }
    }

    #[test]
    fn save_cookies_creates_account_with_session_key() {
        let tmp = TempDir::new().unwrap();
        let am = AccountManager::new(tmp.path()).unwrap();
        let cookies = vec![cookie("sessionKey", "sk-abc"), cookie("lastActiveOrg", "org-1")];
        let r = am.save_cookies(cookies).unwrap();
        assert_eq!(r.outcome, SaveOutcome::Created);
        let aid = r.account_id.unwrap();
        assert!(aid.starts_with("acc_"));
        assert_eq!(am.active_id().as_deref(), Some(aid.as_str()));
        let accs = am.all_including_pending();
        assert_eq!(accs.len(), 1);
        assert!(accs[0].pending);
    }

    #[test]
    fn save_cookies_rejects_blacklisted_org() {
        let tmp = TempDir::new().unwrap();
        let am = AccountManager::new(tmp.path()).unwrap();
        // Manually seed removed_orgs.
        let meta = AccountsMeta {
            removed_orgs: vec!["org-bad".into()],
            ..Default::default()
        };
        am.save_meta(&meta).unwrap();
        let r = am
            .save_cookies(vec![cookie("sessionKey", "x"), cookie("lastActiveOrg", "org-bad")])
            .unwrap();
        assert_eq!(r.outcome, SaveOutcome::Rejected);
        assert!(r.account_id.is_none());
        assert_eq!(am.all_including_pending().len(), 0);
    }

    #[test]
    fn unblock_bypass_lets_blacklisted_org_through() {
        let tmp = TempDir::new().unwrap();
        let am = AccountManager::new(tmp.path()).unwrap();
        am.save_meta(&AccountsMeta {
            removed_orgs: vec!["org-bad".into()],
            ..Default::default()
        })
        .unwrap();
        am.unblock_next_save(60);
        let r = am
            .save_cookies(vec![cookie("sessionKey", "x"), cookie("lastActiveOrg", "org-bad")])
            .unwrap();
        assert_eq!(r.outcome, SaveOutcome::Created);
        // Bypass consumed → org dropped from blacklist.
        let meta = am.load_meta();
        assert!(meta.removed_orgs.is_empty());
        // And bypass window is closed.
        assert!(!am.bypass_active());
    }

    #[test]
    fn refresh_keeps_existing_account_id_via_stable_cookie_match() {
        let tmp = TempDir::new().unwrap();
        let am = AccountManager::new(tmp.path()).unwrap();
        // Initial save → creates acc and sets pending.
        let r1 = am
            .save_cookies(vec![cookie("sessionKey", "sk1"), cookie("lastActiveOrg", "org-A")])
            .unwrap();
        let aid = r1.account_id.unwrap();
        // Confirm with email so find_by_stable_cookies will consider it.
        am.update_info(&aid, Some("e@x"), None, None, None).unwrap();
        am.confirm(&aid).unwrap();
        // Different sessionKey but same lastActiveOrg → match by org.
        let r2 = am
            .save_cookies(vec![cookie("sessionKey", "sk2-new"), cookie("lastActiveOrg", "org-A")])
            .unwrap();
        assert_eq!(r2.outcome, SaveOutcome::Refreshed);
        assert_eq!(r2.account_id.as_deref(), Some(aid.as_str()));
    }

    #[test]
    fn remove_blacklists_org_and_picks_next_active() {
        let tmp = TempDir::new().unwrap();
        let am = AccountManager::new(tmp.path()).unwrap();
        let r1 = am
            .save_cookies(vec![cookie("sessionKey", "s1"), cookie("lastActiveOrg", "org-1")])
            .unwrap();
        let r2 = am
            .save_cookies(vec![cookie("sessionKey", "s2"), cookie("lastActiveOrg", "org-2")])
            .unwrap();
        let id1 = r1.account_id.unwrap();
        let id2 = r2.account_id.unwrap();
        am.switch_to(&id1).unwrap();
        am.remove(&id1).unwrap();
        let meta = am.load_meta();
        assert_eq!(meta.active.as_deref(), Some(id2.as_str()));
        assert!(meta.removed_orgs.contains(&"org-1".to_string()));
    }

    #[test]
    fn save_cookies_no_session_key_rejects() {
        let tmp = TempDir::new().unwrap();
        let am = AccountManager::new(tmp.path()).unwrap();
        let r = am.save_cookies(vec![cookie("noise", "x")]).unwrap();
        assert_eq!(r.outcome, SaveOutcome::Rejected);
    }

    #[test]
    fn cc_tokens_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let am = AccountManager::new(tmp.path()).unwrap();
        let r = am
            .save_cookies(vec![cookie("sessionKey", "s"), cookie("lastActiveOrg", "o")])
            .unwrap();
        let aid = r.account_id.unwrap();
        let tok = CcTokens {
            access_token: "sk-ant-oat01-AAA".into(),
            refresh_token: "sk-ant-ort01-BBB".into(),
            expires_at: 1_777_443_825_528,
            scopes: vec!["user:profile".into()],
            subscription_type: "max".into(),
            rate_limit_tier: "default_claude_max_20x".into(),
        };
        am.update_cc_tokens(&aid, tok.clone()).unwrap();
        let got = am.cc_tokens(&aid).unwrap();
        assert_eq!(got.access_token, "sk-ant-oat01-AAA");
        assert_eq!(got.expires_at, 1_777_443_825_528);
    }
}
