//! Proxy URL persistence and effective-proxy decision (direct probe + cache).
//!
//! Mirrors `_load_proxy_url`, `_save_proxy_url`, `_probe_direct_ok`,
//! `_effective_proxy_url` from `usage_monitor.py`.

use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use once_cell::sync::Lazy;

use crate::types::ProxyConfig;

const RECHECK_SECS: u64 = 600;

#[derive(Debug, Clone)]
struct ProxyDecision {
    /// "" = use direct, otherwise the proxy URL to use now.
    url: String,
    checked_at: Option<Instant>,
}

static DECISION: Lazy<Mutex<ProxyDecision>> = Lazy::new(|| {
    Mutex::new(ProxyDecision {
        url: String::new(),
        checked_at: None,
    })
});

/// Read configured proxy URL from `proxy.json` or env (`HTTPS_PROXY` / `HTTP_PROXY`).
/// Empty string when nothing is configured.
pub fn load_proxy_url(app_dir: &Path) -> String {
    let path = crate::paths::proxy_file(app_dir);
    if path.exists() {
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str::<ProxyConfig>(&s) {
                let url = cfg.url.trim().to_string();
                if !url.is_empty() {
                    return url;
                }
            }
        }
    }
    std::env::var("HTTPS_PROXY")
        .or_else(|_| std::env::var("HTTP_PROXY"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Persist proxy URL (empty clears it). Invalidates the in-memory decision cache.
pub fn save_proxy_url(app_dir: &Path, url: &str) -> anyhow::Result<()> {
    let cfg = ProxyConfig { url: url.trim().to_string() };
    let body = serde_json::to_string_pretty(&cfg)?;
    crate::account_manager::atomic_write(&crate::paths::proxy_file(app_dir), body.as_bytes())?;
    invalidate_cache();
    Ok(())
}

/// Force a re-probe on the next `effective_proxy_url` call.
pub fn invalidate_cache() {
    if let Ok(mut d) = DECISION.lock() {
        d.checked_at = None;
    }
}

/// True if `claude.ai/api/organizations` is reachable directly within 5s.
/// Any HTTP response (including 401/403) means the network path is fine —
/// we only need a proxy when the connection itself fails (or 451 region-blocks).
pub async fn probe_direct_ok() -> bool {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .no_proxy()
        .user_agent("Mozilla/5.0")
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get("https://claude.ai/api/organizations").send().await {
        Ok(r) => {
            let s = r.status().as_u16();
            s != 451 && s < 500
        }
        Err(_) => false,
    }
}

/// Resolve the proxy URL to use for the next request: empty if direct works,
/// configured proxy otherwise. Caches the decision for 10 min.
pub async fn effective_proxy_url(app_dir: &Path) -> String {
    let configured = load_proxy_url(app_dir);
    if configured.is_empty() {
        return String::new();
    }

    {
        let d = DECISION.lock().unwrap();
        if let Some(when) = d.checked_at {
            if when.elapsed().as_secs() < RECHECK_SECS {
                return d.url.clone();
            }
        }
    }

    let direct_ok = probe_direct_ok().await;
    let chosen = if direct_ok { String::new() } else { configured };
    let mut d = DECISION.lock().unwrap();
    d.url = chosen.clone();
    d.checked_at = Some(Instant::now());
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_save_roundtrip() {
        let tmp = TempDir::new().unwrap();
        save_proxy_url(tmp.path(), "http://127.0.0.1:10808").unwrap();
        assert_eq!(load_proxy_url(tmp.path()), "http://127.0.0.1:10808");
        // Saving an empty/whitespace URL must persist `{"url": ""}` so the file
        // no longer "wins" — load then falls through to env. We assert on the
        // written content (env-independent).
        save_proxy_url(tmp.path(), "  ").unwrap();
        let raw = std::fs::read_to_string(crate::paths::proxy_file(tmp.path())).unwrap();
        assert!(raw.contains("\"url\": \"\""), "got: {raw}");
    }
}
