//! TLS fingerprint smoke test.
//!
//! Reads cookies from `accounts/<active>.json` (or argv[1]), hits
//! `claude.ai/api/organizations` 5 times with plain rustls, and reports
//! how many requests succeeded with HTTP 200. If 5/5 work, we don't need
//! Chrome impersonation — saves ~15 MB of binary size and a NASM dependency.
//!
//! Run from the repo root:
//!   cd rust && cargo run --example tls_probe -- ../accounts/acc_273bc5755e.json

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use claude_monitor::types::{StorageState, last_active_org};

#[tokio::main]
async fn main() -> Result<()> {
    let arg = std::env::args().nth(1);
    let path = match arg {
        Some(p) => PathBuf::from(p),
        None => default_active_account_file()?,
    };
    eprintln!("[probe] using cookie file: {}", path.display());

    let raw = std::fs::read_to_string(&path).context("read cookie file")?;
    let storage: StorageState = serde_json::from_str(&raw).context("parse cookie file")?;

    let mut header = String::new();
    let mut session_seen = false;
    let mut org_uuid_from_cookie = String::new();
    for c in &storage.cookies {
        if c.domain.contains("claude.ai") || c.domain.starts_with(".claude.ai") {
            if !header.is_empty() {
                header.push_str("; ");
            }
            header.push_str(&format!("{}={}", c.name, c.value));
            if c.name == "sessionKey" {
                session_seen = true;
            }
        }
    }
    if let Some(o) = last_active_org(&storage.cookies) {
        org_uuid_from_cookie = o.to_string();
    }
    if !session_seen {
        anyhow::bail!("no sessionKey cookie found — cannot probe");
    }

    eprintln!(
        "[probe] cookies built ({} bytes), lastActiveOrg = {}",
        header.len(),
        if org_uuid_from_cookie.is_empty() {
            "<none>"
        } else {
            &org_uuid_from_cookie
        }
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        )
        .build()?;

    let mut ok_count = 0;
    let mut last_status = 0;
    let endpoints = [
        ("/api/organizations", true),
        ("/api/account", true),
        // Usage endpoint depends on org UUID
    ];

    for round in 1..=3 {
        for (path, _) in &endpoints {
            let url = format!("https://claude.ai{}", path);
            let resp = client
                .get(&url)
                .header("Cookie", &header)
                .header("Accept", "application/json")
                .header("Accept-Language", "en-US,en;q=0.9")
                .send()
                .await;
            match resp {
                Ok(r) => {
                    last_status = r.status().as_u16();
                    let ok = r.status().as_u16() == 200;
                    if ok {
                        ok_count += 1;
                    }
                    eprintln!(
                        "[probe] round {} GET {} → {}",
                        round,
                        path,
                        r.status()
                    );
                    if ok && *path == "/api/organizations" && org_uuid_from_cookie.is_empty() {
                        if let Ok(orgs) = r.json::<Vec<serde_json::Value>>().await {
                            if let Some(u) = orgs.first().and_then(|o| o.get("uuid")).and_then(|v| v.as_str()) {
                                org_uuid_from_cookie = u.to_string();
                                eprintln!("[probe] picked org_uuid from response: {}", org_uuid_from_cookie);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[probe] round {} {} → ERR {}", round, path, e);
                }
            }
        }
    }

    // One usage request — the real endpoint we care about.
    if !org_uuid_from_cookie.is_empty() {
        let url = format!(
            "https://claude.ai/api/organizations/{}/usage",
            org_uuid_from_cookie
        );
        let resp = client
            .get(&url)
            .header("Cookie", &header)
            .header("Accept", "application/json")
            .send()
            .await;
        match resp {
            Ok(r) => {
                last_status = r.status().as_u16();
                let s = r.status();
                eprintln!("[probe] GET /usage → {}", s);
                if s.as_u16() == 200 {
                    ok_count += 1;
                    if let Ok(body) = r.text().await {
                        let preview: String = body.chars().take(200).collect();
                        eprintln!("[probe] /usage preview: {preview}…");
                    }
                }
            }
            Err(e) => eprintln!("[probe] /usage → ERR {e}"),
        }
    }

    eprintln!(
        "\n[probe] summary: {} OK responses, last status = {}",
        ok_count, last_status
    );
    if ok_count >= 6 {
        eprintln!(
            "[probe] VERDICT: claude.ai accepts plain rustls TLS. \
             No Chrome fingerprinting needed."
        );
    } else if last_status == 403 || last_status == 401 {
        eprintln!(
            "[probe] VERDICT: got 401/403 — may need Chrome TLS fingerprinting (rquest+BoringSSL+NASM) \
             OR cookies are expired."
        );
    } else {
        eprintln!("[probe] VERDICT: inconclusive. Investigate manually.");
    }
    Ok(())
}

fn default_active_account_file() -> Result<PathBuf> {
    // Walk up two levels: examples/ → rust/ → ClaudeMonitor/
    let app_dir = std::env::current_dir()?
        .parent()
        .context("no parent of cwd")?
        .to_path_buf();
    let meta_path = app_dir.join("accounts_meta.json");
    let raw = std::fs::read_to_string(&meta_path)
        .with_context(|| format!("read {}", meta_path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    let aid = v
        .get("active")
        .and_then(|x| x.as_str())
        .context("no active account in accounts_meta.json")?;
    Ok(app_dir.join("accounts").join(format!("{aid}.json")))
}
