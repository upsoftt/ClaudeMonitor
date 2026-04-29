//! claude.ai API client — usage, identity, ping (auto-kickstart),
//! and the public Statuspage incidents feed.
//!
//! All requests go through `http_client::with_proxy_fallback` so a configured
//! proxy is used only when direct access fails.

use std::path::Path;

use anyhow::Result;
use serde_json::json;
use uuid::Uuid;

use crate::http_client::{
    apply_default_headers, make_client, with_proxy_fallback, ApiError, SessionContext,
};
use crate::types::{
    plan_from_org, AccountInfo, Incident, IncidentsPayload, OrgInfo, UsageResponse,
};

/// Resolved identity for an account.
#[derive(Debug, Clone)]
pub struct Identity {
    pub email: String,
    pub display_name: String,
    pub plan: String,
    pub uuid: String,
}

/// Fetch `/api/organizations/<lastActiveOrg>/usage`. The org UUID comes from
/// the cookie jar (it's the user's *active* org), NOT from `account.uuid`
/// (that's the user UUID and gives 404 here).
pub async fn fetch_usage(app_dir: &Path, ctx: &SessionContext) -> Result<UsageResponse> {
    if !ctx.has_session() {
        return Err(ApiError::NoSession.into());
    }
    if ctx.last_active_org.is_empty() {
        return Err(ApiError::NoOrg.into());
    }
    let cookie = ctx.cookie_header.clone();
    let org = ctx.last_active_org.clone();
    with_proxy_fallback(app_dir, move |client| {
        let cookie = cookie.clone();
        let org = org.clone();
        async move {
            let resp = apply_default_headers(
                client.get(format!("https://claude.ai/api/organizations/{org}/usage")),
                &cookie,
            )
            .send()
            .await
            .map_err(ApiError::Transport)?;
            let s = resp.status();
            match s.as_u16() {
                200 => Ok(resp.json::<UsageResponse>().await.map_err(ApiError::Transport)?),
                401 | 403 => Err(ApiError::SessionExpired(s.as_u16()).into()),
                451 => Err(ApiError::RegionBlocked.into()),
                code => {
                    let body = resp.text().await.unwrap_or_default();
                    Err(ApiError::Http(code, body).into())
                }
            }
        }
    })
    .await
}

/// Resolve email/name/plan/uuid via `/api/account` + `/api/organizations`.
pub async fn fetch_identity(app_dir: &Path, ctx: &SessionContext) -> Result<Identity> {
    if !ctx.has_session() {
        return Err(ApiError::NoSession.into());
    }
    let cookie = ctx.cookie_header.clone();
    with_proxy_fallback(app_dir, move |client| {
        let cookie = cookie.clone();
        async move {
            // /api/account
            let resp = apply_default_headers(
                client.get("https://claude.ai/api/account"),
                &cookie,
            )
            .send()
            .await
            .map_err(ApiError::Transport)?;
            let acc: AccountInfo = match resp.status().as_u16() {
                200 => resp.json().await.map_err(ApiError::Transport)?,
                401 | 403 => return Err(ApiError::SessionExpired(resp.status().as_u16()).into()),
                code => {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(ApiError::Http(code, body).into());
                }
            };
            // /api/organizations — pick first user-billable org.
            let resp2 = apply_default_headers(
                client.get("https://claude.ai/api/organizations"),
                &cookie,
            )
            .send()
            .await
            .map_err(ApiError::Transport)?;
            let mut plan = String::new();
            if resp2.status().as_u16() == 200 {
                let orgs: Vec<OrgInfo> = resp2.json().await.map_err(ApiError::Transport)?;
                let chosen = orgs.iter().find(|o| o.billing_type.is_some()).or_else(|| orgs.first());
                if let Some(o) = chosen {
                    plan = plan_from_org(o);
                }
            }
            let display = if !acc.display_name.is_empty() {
                acc.display_name.clone()
            } else {
                acc.full_name.clone()
            };
            Ok(Identity {
                email: acc.email_address,
                display_name: display,
                plan,
                uuid: acc.uuid,
            })
        }
    })
    .await
}

/// "Auto-kickstart" — create a chat conversation, send a 1-token completion,
/// then delete the conversation. Used when an account has no active 5h
/// session and the user wants to start the timer manually.
pub async fn ping(app_dir: &Path, ctx: &SessionContext) -> Result<()> {
    if !ctx.has_session() {
        return Err(ApiError::NoSession.into());
    }
    let cookie = ctx.cookie_header.clone();
    with_proxy_fallback(app_dir, move |client| {
        let cookie = cookie.clone();
        async move {
            // Pick org from /api/organizations (we want a fresh value, not the cookie).
            let resp = apply_default_headers(
                client.get("https://claude.ai/api/organizations"),
                &cookie,
            )
            .send()
            .await
            .map_err(ApiError::Transport)?;
            if resp.status().as_u16() != 200 {
                return Err(ApiError::SessionExpired(resp.status().as_u16()).into());
            }
            let orgs: Vec<serde_json::Value> = resp.json().await.map_err(ApiError::Transport)?;
            let org_uuid = orgs
                .first()
                .and_then(|o| o.get("uuid"))
                .and_then(|v| v.as_str())
                .ok_or(ApiError::NoOrg)?
                .to_string();

            let conv_uuid = Uuid::new_v4().to_string();
            let create = apply_default_headers(
                client.post(format!(
                    "https://claude.ai/api/organizations/{org_uuid}/chat_conversations"
                )),
                &cookie,
            )
            .header("Content-Type", "application/json")
            .json(&json!({"name": "", "uuid": conv_uuid}))
            .send()
            .await
            .map_err(ApiError::Transport)?;
            let s = create.status().as_u16();
            if s != 200 && s != 201 {
                let body = create.text().await.unwrap_or_default();
                return Err(ApiError::Http(s, body).into());
            }
            let conv_resp: serde_json::Value =
                create.json().await.map_err(ApiError::Transport)?;
            let conv_uuid = conv_resp
                .get("uuid")
                .and_then(|v| v.as_str())
                .unwrap_or(&conv_uuid)
                .to_string();

            let _ = apply_default_headers(
                client.post(format!(
                    "https://claude.ai/api/organizations/{org_uuid}/chat_conversations/{conv_uuid}/completion"
                )),
                &cookie,
            )
            .header("Content-Type", "application/json")
            .json(&json!({
                "prompt": "\n\nHuman: hi\n\nAssistant:",
                "model": "claude-sonnet-4-20250514",
                "max_tokens_to_sample": 5,
            }))
            .send()
            .await
            .map_err(ApiError::Transport)?;

            let _ = apply_default_headers(
                client.delete(format!(
                    "https://claude.ai/api/organizations/{org_uuid}/chat_conversations/{conv_uuid}"
                )),
                &cookie,
            )
            .send()
            .await
            .map_err(ApiError::Transport)?;

            Ok(())
        }
    })
    .await
}

/// Public Statuspage incidents (no auth, no proxy logic — runs on the public
/// internet and is extremely cheap).
pub async fn fetch_incidents() -> Result<Vec<Incident>> {
    let client = make_client(None)?;
    let resp = client
        .get("https://status.claude.com/api/v2/incidents/unresolved.json")
        .header("Accept", "application/json")
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(ApiError::Http(resp.status().as_u16(), String::new()).into());
    }
    let payload: IncidentsPayload = resp.json().await?;
    Ok(payload.incidents)
}
