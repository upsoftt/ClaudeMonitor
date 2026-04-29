//! Serde schemas for persisted data and claude.ai API responses.
//!
//! Field names follow the on-disk and on-wire JSON exactly so we can
//! round-trip with Python files (`accounts_meta.json`, `accounts/*.json`)
//! without migration.

use serde::{Deserialize, Serialize};

// ─────────────────────────── On-disk: accounts_meta.json ─────────────────────

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AccountsMeta {
    /// Active account id (or `None`).
    #[serde(default)]
    pub active: Option<String>,

    #[serde(default)]
    pub accounts: Vec<Account>,

    /// Org UUIDs that the user explicitly removed — bridge pushes for these
    /// orgs are silently ignored.
    #[serde(default)]
    pub removed_orgs: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,

    #[serde(default)]
    pub email: String,

    #[serde(default)]
    pub name: String,

    #[serde(default)]
    pub plan: String,

    #[serde(default)]
    pub uuid: String,

    /// ISO-8601 timestamp of when the account was first added.
    #[serde(default)]
    pub added_at: String,

    /// `true` while identity is being resolved — hidden from UI until cleared.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pending: bool,

    /// Claude Code OAuth tokens captured from `~/.claude/.credentials.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cc_tokens: Option<CcTokens>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CcTokens {
    #[serde(rename = "accessToken", default)]
    pub access_token: String,

    #[serde(rename = "refreshToken", default)]
    pub refresh_token: String,

    /// Unix-millis expiry.
    #[serde(rename = "expiresAt", default)]
    pub expires_at: u64,

    #[serde(default)]
    pub scopes: Vec<String>,

    #[serde(rename = "subscriptionType", default)]
    pub subscription_type: String,

    #[serde(rename = "rateLimitTier", default)]
    pub rate_limit_tier: String,
}

// ─────────────────────────── On-disk: accounts/<id>.json ─────────────────────

/// Playwright-compatible storage_state file. Only `cookies` is used by us;
/// `origins` is preserved verbatim for Playwright re-login compatibility.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StorageState {
    #[serde(default)]
    pub cookies: Vec<Cookie>,

    #[serde(default)]
    pub origins: Vec<serde_json::Value>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,

    #[serde(default)]
    pub value: String,

    #[serde(default)]
    pub domain: String,

    #[serde(default)]
    pub path: String,

    /// Unix seconds (Playwright format). `-1` means session cookie.
    #[serde(default)]
    pub expires: f64,

    #[serde(default, rename = "httpOnly", skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,

    #[serde(rename = "sameSite", default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
}

// ─────────────────────────── On-disk: proxy.json / window_state.json ─────────

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct WindowState {
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub compact: bool,
}

// ─────────────────────────── Wire: claude.ai /api/organizations ──────────────

#[derive(Debug, Default, Clone, Deserialize)]
pub struct OrgInfo {
    pub uuid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub rate_limit_tier: Option<String>,
    #[serde(default)]
    pub billing_type: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<serde_json::Value>,
}

// ─────────────────────────── Wire: /api/account ──────────────────────────────

#[derive(Debug, Default, Clone, Deserialize)]
pub struct AccountInfo {
    #[serde(default)]
    pub email_address: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub uuid: String,
}

// ─────────────────────────── Wire: /api/organizations/<id>/usage ─────────────

/// Whole `/usage` payload (fields we care about).
#[derive(Debug, Default, Clone, Deserialize)]
pub struct UsageResponse {
    #[serde(default)]
    pub five_hour: Option<MetricBucket>,
    #[serde(default)]
    pub seven_day: Option<MetricBucket>,
    #[serde(default)]
    pub seven_day_omelette: Option<MetricBucket>,
    #[serde(default)]
    pub seven_day_opus: Option<MetricBucket>,
    #[serde(default)]
    pub seven_day_sonnet: Option<MetricBucket>,
    #[serde(default)]
    pub seven_day_cowork: Option<MetricBucket>,
    #[serde(default)]
    pub seven_day_oauth_apps: Option<MetricBucket>,
}

/// Real claude.ai `/usage` payload format (verified 2026-04-29):
///   `{"utilization": 85.0, "resets_at": "..."}`
/// `utilization` is already a percentage (0–100+). Older Python code expected
/// `used`/`used_limit` but the live API never sent those — kept here only as
/// optional fallback fields in case of schema drift.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct MetricBucket {
    /// Already a percentage (0.0–100.0+, can exceed 100 if overage allowed).
    #[serde(default)]
    pub utilization: f64,

    /// ISO-8601 reset timestamp. `null` when the feature isn't active for
    /// this account (e.g. `seven_day_omelette` for accounts without Design).
    #[serde(default)]
    pub resets_at: Option<String>,

    // Schema-drift fallbacks (unused on current API).
    #[serde(default)]
    pub used: Option<f64>,
    #[serde(default)]
    pub used_limit: Option<f64>,
}

impl MetricBucket {
    /// Percent used (0.0–100.0+).
    pub fn percent(&self) -> f64 {
        if self.utilization > 0.0 {
            return self.utilization;
        }
        match (self.used, self.used_limit) {
            (Some(u), Some(lim)) if lim > 0.0 => (u / lim) * 100.0,
            _ => 0.0,
        }
    }
}

// ─────────────────────────── Wire: status.claude.com incidents ───────────────

#[derive(Debug, Default, Clone, Deserialize)]
pub struct IncidentsPayload {
    #[serde(default)]
    pub incidents: Vec<Incident>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Incident {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// `investigating` / `identified` / `monitoring` / `resolved` / etc.
    #[serde(default)]
    pub status: String,
    /// `none` / `minor` / `major` / `critical` / `maintenance`.
    #[serde(default)]
    pub impact: String,
    #[serde(default)]
    pub shortlink: String,
    #[serde(default)]
    pub incident_updates: Vec<IncidentUpdate>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct IncidentUpdate {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: String,
}

// ─────────────────────────── Wire: CookieBridge v2 push ──────────────────────

/// `POST /cookies` body (CookieBridge v2 hub schema).
#[derive(Debug, Clone, Deserialize)]
pub struct CookieBridgePush {
    #[serde(rename = "schemaVersion", default)]
    pub schema_version: u32,
    #[serde(default)]
    pub timestamp: Option<i64>,
    #[serde(rename = "profileLabel", default)]
    pub profile_label: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub cookies: Vec<Cookie>,
}

/// Legacy `POST /site-cookies/claude` body (CookieBridge v1).
#[derive(Debug, Clone, Deserialize)]
pub struct LegacyCookiePush {
    #[serde(default)]
    pub cookies: Vec<Cookie>,
}

// ─────────────────────────── Helpers ─────────────────────────────────────────

/// Pull the `lastActiveOrg` cookie value, if present.
pub fn last_active_org<'a>(cookies: &'a [Cookie]) -> Option<&'a str> {
    cookies
        .iter()
        .find(|c| c.name == "lastActiveOrg" && !c.value.is_empty())
        .map(|c| c.value.as_str())
}

/// Pull the `sessionKey` cookie value, if present.
pub fn session_key<'a>(cookies: &'a [Cookie]) -> Option<&'a str> {
    cookies
        .iter()
        .find(|c| c.name == "sessionKey" && !c.value.is_empty())
        .map(|c| c.value.as_str())
}

/// Map `rate_limit_tier` (e.g. `default_claude_max_20x`) → human plan name.
/// Falls back through capabilities → billing_type → "Free".
pub fn plan_from_org(org: &OrgInfo) -> String {
    if let Some(tier) = &org.rate_limit_tier {
        let t = tier.to_lowercase();
        if t.contains("max_20x") {
            return "Max200".into();
        }
        if t.contains("max_5x") {
            return "Max100".into();
        }
        if t.contains("max") {
            return "Max".into();
        }
        if t.contains("pro") {
            return "Pro".into();
        }
        if t.contains("team") {
            return "Team".into();
        }
        if t.contains("enterprise") || t.contains("ent") {
            return "Ent".into();
        }
    }
    // Capabilities fallback.
    let cap_names: Vec<String> = org
        .capabilities
        .iter()
        .filter_map(|c| match c {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Object(o) => o.get("name").and_then(|v| v.as_str()).map(String::from),
            _ => None,
        })
        .collect();
    for (cap, plan) in &[
        ("claude_max", "Max"),
        ("claude_pro", "Pro"),
        ("teams", "Team"),
        ("enterprise", "Ent"),
    ] {
        if cap_names.iter().any(|n| n == cap) {
            return (*plan).into();
        }
    }
    if matches!(org.billing_type.as_deref(), Some("stripe_subscription")) {
        return "Pro".into();
    }
    "Free".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_accounts_meta() {
        // Subset of real accounts_meta.json from the running deployment.
        let json = r#"{
            "active": "acc_273bc5755e",
            "accounts": [{
                "id": "acc_5f410b45cf",
                "email": "upsoftt@gmail.com",
                "name": "Pavel",
                "plan": "Max200",
                "added_at": "2026-04-18T00:30:05.508924",
                "uuid": "421427ac-2e64-421b-9f27-5cbfad4edb68",
                "cc_tokens": {
                    "accessToken": "sk-ant-oat01-AAA",
                    "refreshToken": "sk-ant-ort01-BBB",
                    "expiresAt": 1777443825528,
                    "scopes": ["user:profile"],
                    "subscriptionType": "max",
                    "rateLimitTier": "default_claude_max_20x"
                }
            }],
            "removed_orgs": ["abc"]
        }"#;
        let meta: AccountsMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.active.as_deref(), Some("acc_273bc5755e"));
        assert_eq!(meta.accounts.len(), 1);
        assert_eq!(meta.accounts[0].email, "upsoftt@gmail.com");
        let cc = meta.accounts[0].cc_tokens.as_ref().unwrap();
        assert_eq!(cc.expires_at, 1777443825528);
        assert_eq!(meta.removed_orgs, vec!["abc"]);
    }

    #[test]
    fn round_trips_account_without_cc_tokens() {
        let meta = AccountsMeta {
            active: Some("a".into()),
            accounts: vec![Account {
                id: "a".into(),
                email: "x@y".into(),
                ..Default::default()
            }],
            removed_orgs: vec![],
        };
        let s = serde_json::to_string(&meta).unwrap();
        // pending=false and cc_tokens=None should be skipped
        assert!(!s.contains("\"pending\""));
        assert!(!s.contains("\"cc_tokens\""));
    }

    #[test]
    fn metric_percent_from_utilization_field() {
        // Real API shape: `{"utilization": 85.0, "resets_at": "..."}`
        let json = r#"{"utilization": 85.0, "resets_at": "2026-04-29T15:10:01.248484+00:00"}"#;
        let m: MetricBucket = serde_json::from_str(json).unwrap();
        assert!((m.percent() - 85.0).abs() < 1e-6);
        assert_eq!(m.resets_at.as_deref(), Some("2026-04-29T15:10:01.248484+00:00"));
    }

    #[test]
    fn metric_percent_legacy_fallback() {
        // If API ever switched back to used/used_limit shape, fallback works.
        let json = r#"{"used": 50.0, "used_limit": 200.0}"#;
        let m: MetricBucket = serde_json::from_str(json).unwrap();
        assert!((m.percent() - 25.0).abs() < 1e-6);
    }

    #[test]
    fn metric_zero_default() {
        let zero = MetricBucket::default();
        assert_eq!(zero.percent(), 0.0);
    }

    #[test]
    fn plan_from_max20x_tier() {
        let mut o = OrgInfo::default();
        o.rate_limit_tier = Some("default_claude_max_20x".into());
        assert_eq!(plan_from_org(&o), "Max200");
        o.rate_limit_tier = Some("default_claude_max_5x".into());
        assert_eq!(plan_from_org(&o), "Max100");
        o.rate_limit_tier = None;
        o.billing_type = Some("stripe_subscription".into());
        assert_eq!(plan_from_org(&o), "Pro");
    }

    #[test]
    fn cookie_helpers() {
        let cookies = vec![
            Cookie { name: "sessionKey".into(), value: "sk-x".into(), ..Default::default() },
            Cookie { name: "lastActiveOrg".into(), value: "abc".into(), ..Default::default() },
            Cookie { name: "noise".into(), value: "n".into(), ..Default::default() },
        ];
        assert_eq!(session_key(&cookies), Some("sk-x"));
        assert_eq!(last_active_org(&cookies), Some("abc"));
    }
}
