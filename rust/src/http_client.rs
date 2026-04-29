//! Shared HTTP client setup for claude.ai requests.
//!
//! Build a `reqwest::Client` with sensible defaults (Chrome-y UA, gzip/brotli,
//! optional proxy). Pass cookies as a manually-built `Cookie` header so the
//! caller never has to share a CookieStore across accounts.

use std::time::Duration;

use anyhow::Result;

use crate::types::{Cookie, StorageState};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Build a reqwest client. `proxy` of `None` or `Some("")` means direct.
pub fn make_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(UA)
        .https_only(false);
    match proxy {
        Some(p) if !p.trim().is_empty() => {
            b = b.proxy(reqwest::Proxy::all(p.trim())?);
        }
        _ => {
            b = b.no_proxy();
        }
    }
    Ok(b.build()?)
}

/// Concatenate claude.ai cookies into a single `Cookie:` header value.
pub fn cookie_header(cookies: &[Cookie]) -> String {
    let mut out = String::new();
    for c in cookies {
        if !c.domain.contains("claude.ai") {
            continue;
        }
        if !out.is_empty() {
            out.push_str("; ");
        }
        out.push_str(&c.name);
        out.push('=');
        out.push_str(&c.value);
    }
    out
}

/// Read storage_state from disk and return cookie header + lastActiveOrg.
pub fn load_account_session(path: &std::path::Path) -> Result<SessionContext> {
    let raw = std::fs::read_to_string(path)?;
    let storage: StorageState = serde_json::from_str(&raw)?;
    let cookie = cookie_header(&storage.cookies);
    let last_active_org = crate::types::last_active_org(&storage.cookies)
        .map(String::from)
        .unwrap_or_default();
    let session_key = crate::types::session_key(&storage.cookies)
        .map(String::from)
        .unwrap_or_default();
    Ok(SessionContext {
        cookie_header: cookie,
        last_active_org,
        session_key,
    })
}

#[derive(Debug, Clone)]
pub struct SessionContext {
    pub cookie_header: String,
    pub last_active_org: String,
    pub session_key: String,
}

impl SessionContext {
    pub fn has_session(&self) -> bool {
        !self.session_key.is_empty()
    }
}

/// Apply our default headers to a request builder.
pub fn apply_default_headers(rb: reqwest::RequestBuilder, cookie: &str) -> reqwest::RequestBuilder {
    rb.header("Cookie", cookie)
        .header("Accept", "application/json")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Origin", "https://claude.ai")
        .header("Referer", "https://claude.ai/")
}

/// Run `op` with direct connection first; on transport error retry through the
/// configured proxy. Auth errors (401/403/451) are NOT retried — proxies don't
/// fix bad cookies.
pub async fn with_proxy_fallback<T, F, Fut>(
    app_dir: &std::path::Path,
    op: F,
) -> Result<T>
where
    F: Fn(reqwest::Client) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let direct_err: anyhow::Error = match op(make_client(None)?).await {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };

    // Don't retry through proxy on session-class errors — caller already
    // wraps them in `ApiError::SessionExpired`. We only retry transport-class
    // errors (timeouts, DNS, connection refused, region-block 451).
    if let Some(api_err) = direct_err.downcast_ref::<ApiError>() {
        if api_err.is_auth_failure() {
            return Err(direct_err);
        }
    }

    let proxy = crate::proxy::load_proxy_url(app_dir);
    if proxy.is_empty() {
        return Err(direct_err);
    }
    tracing::warn!(direct_err = %direct_err, proxy = %proxy, "direct failed, retrying via proxy");
    op(make_client(Some(&proxy))?).await
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("session expired (HTTP {0})")]
    SessionExpired(u16),
    #[error("region blocked (HTTP 451)")]
    RegionBlocked,
    #[error("HTTP {0}: {1}")]
    Http(u16, String),
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("no organization in account")]
    NoOrg,
    #[error("missing session cookie")]
    NoSession,
}

impl ApiError {
    pub fn is_auth_failure(&self) -> bool {
        matches!(self, ApiError::SessionExpired(_) | ApiError::NoSession)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_header_filters_to_claude_ai() {
        let cookies = vec![
            Cookie { name: "sessionKey".into(), value: "sk".into(), domain: ".claude.ai".into(), ..Default::default() },
            Cookie { name: "evil".into(), value: "x".into(), domain: ".attacker.test".into(), ..Default::default() },
            Cookie { name: "lastActiveOrg".into(), value: "org".into(), domain: "claude.ai".into(), ..Default::default() },
        ];
        let h = cookie_header(&cookies);
        assert!(h.contains("sessionKey=sk"));
        assert!(h.contains("lastActiveOrg=org"));
        assert!(!h.contains("evil"));
    }
}
