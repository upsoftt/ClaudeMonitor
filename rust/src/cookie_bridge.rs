//! HTTP receiver on `localhost:19225` for HMAC-verified cookie pushes from
//! Cookie Bridge Hub (`127.0.0.1:19280`).
//!
//! Routes:
//!   GET  /health    → {"ok":true,"app":"ClaudeMonitor"}
//!   POST /cookies   → Hub-signed push body, HMAC-SHA256 over canonical:
//!                         <ts>\n<nonce>\nPOST\n/cookies\n<body bytes>
//!                     Headers required:
//!                         X-CB-Timestamp, X-CB-Nonce, X-CB-Signature: sha256=<hex>
//!                     Accepted body shapes (any of):
//!                         {"snapshots":[{"cookies":[…],"domain":".claude.ai", …}, …]}
//!                         {"snapshot":{"cookies":[…], …}}
//!                         {"cookies":[…]}
//!   POST /shutdown  → graceful self-shutdown (used by port-capture; no auth).
//!
//! On startup we ensure registration with the Hub:
//!   1. Try to load `<app_dir>/cb_consumer.json` (`{"id":…, "secret":…}`).
//!   2. If absent, POST `/register` with manifest
//!         {id, displayName, domains:[".claude.ai"], profiles:["*"],
//!          receiver:{url:"http://127.0.0.1:19225/cookies"}, schemaVersion:"1.0"}
//!      and persist `{id, secret}` returned by the Hub.
//!
//! Pull-mode is rejected by the Hub when `profiles:["*"]`, and ClaudeMonitor
//! legitimately consumes whichever Chrome profile the user logs into — so we
//! use push-mode with HMAC verification on incoming traffic.
//!
//! Port-capture (per global CLAUDE.md): same-port replacement, never port+1.
//! See `CookieBridge/CONSUMER_GUIDE.md` for protocol details.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::mpsc;

use crate::types::Cookie;

pub const BRIDGE_PORT: u16 = 19225;
const HUB_URL: &str = "http://127.0.0.1:19280";
const CONSUMER_ID: &str = "claude-monitor";
const CONSUMER_NAME: &str = "Claude Monitor";
const SECRET_FILE: &str = "cb_consumer.json";
const TS_TOLERANCE_SEC: i64 = 90;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
struct AppState {
    tx: mpsc::Sender<CookieEvent>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    secret: Arc<String>,
}

#[derive(Debug, Clone)]
pub struct CookieEvent {
    /// Profile label from the Hub snapshot, empty when absent.
    pub profile_label: String,
    pub event: String,
    pub cookies: Vec<Cookie>,
}

#[derive(Serialize, Deserialize)]
struct StoredCredentials {
    id: String,
    secret: String,
}

#[derive(Serialize)]
struct RegisterManifest<'a> {
    id: &'a str,
    #[serde(rename = "displayName")]
    display_name: &'a str,
    domains: &'a [&'a str],
    profiles: &'a [&'a str],
    receiver: ReceiverSpec<'a>,
    #[serde(rename = "schemaVersion")]
    schema_version: &'a str,
}

#[derive(Serialize)]
struct ReceiverSpec<'a> {
    url: &'a str,
}

#[derive(Deserialize)]
struct RegisterResponse {
    id: Option<String>,
    secret: Option<String>,
}

/// Hub push body shapes we accept. `untagged` walks variants in declaration
/// order and picks the first whose required (non-`default`) fields are present
/// — so each variant must keep a unique discriminator field as required.
#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum PushBody {
    /// Multi-account: `{"snapshots": [{...}, …]}`.
    Snapshots { snapshots: Vec<SnapshotEntry> },
    /// Single-snapshot envelope: `{"snapshot": {…}}`.
    OneSnapshot { snapshot: SnapshotEntry },
    /// Legacy raw envelope: `{"cookies": [...], "profileLabel": …}`.
    Bare {
        cookies: Vec<Cookie>,
        #[serde(default, rename = "profileLabel")]
        profile_label: String,
        #[serde(default)]
        event: String,
    },
}

#[derive(Deserialize, Debug)]
struct SnapshotEntry {
    #[serde(default)]
    cookies: Vec<Cookie>,
    #[serde(default, rename = "profileLabel")]
    profile_label: String,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    event: String,
}

/// Bind to 127.0.0.1:19225 and serve until shutdown is requested. Cookie
/// events are forwarded over `tx`. Side-effect: persists `cb_consumer.json`
/// in `app_dir` on first run after Hub registration.
pub async fn run(tx: mpsc::Sender<CookieEvent>, app_dir: PathBuf) -> Result<()> {
    let (id, secret) = ensure_registered(&app_dir).await?;
    tracing::info!(consumer_id = %id, "CookieBridge consumer ready");

    capture_port().await?;

    let (sd_tx, mut sd_rx) = tokio::sync::watch::channel(false);
    let state = AppState {
        tx,
        shutdown_tx: sd_tx,
        secret: Arc::new(secret),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/cookies", post(post_cookies).options(cors_preflight))
        .route("/shutdown", post(post_shutdown).get(post_shutdown))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], BRIDGE_PORT));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, "cookie_bridge listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = sd_rx.changed().await;
            tracing::info!("cookie_bridge graceful shutdown");
        })
        .await?;
    Ok(())
}

// ───────────────────────────── handlers ─────────────────────────────────────

async fn health() -> impl IntoResponse {
    let body = Json(serde_json::json!({"ok": true, "app": "ClaudeMonitor"}));
    cors(body.into_response())
}

async fn cors_preflight() -> impl IntoResponse {
    cors(StatusCode::NO_CONTENT.into_response())
}

async fn post_shutdown(State(s): State<AppState>) -> impl IntoResponse {
    tracing::info!("/shutdown requested by remote");
    let _ = s.shutdown_tx.send(true);
    cors(Json(serde_json::json!({"ok": true})).into_response())
}

async fn post_cookies(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let ts    = headers.get("x-cb-timestamp").and_then(|v| v.to_str().ok()).unwrap_or("");
    let nonce = headers.get("x-cb-nonce").and_then(|v| v.to_str().ok()).unwrap_or("");
    let sig   = headers.get("x-cb-signature").and_then(|v| v.to_str().ok()).unwrap_or("");

    if !verify_hmac(&s.secret, ts, nonce, "POST", "/cookies", &body, sig) {
        tracing::warn!(
            ts, nonce_len = nonce.len(), sig_prefix = &sig[..sig.len().min(14)],
            "CookieBridge push REJECTED — bad HMAC"
        );
        let mut resp = StatusCode::UNAUTHORIZED.into_response();
        resp = cors(resp);
        return resp;
    }

    let payload: PushBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "CookieBridge push: bad JSON");
            let mut resp = StatusCode::BAD_REQUEST.into_response();
            resp = cors(resp);
            return resp;
        }
    };

    let snapshots = match payload {
        PushBody::Snapshots { snapshots } => snapshots,
        PushBody::OneSnapshot { snapshot } => vec![snapshot],
        PushBody::Bare { cookies, profile_label, event } => vec![SnapshotEntry {
            cookies,
            profile_label,
            domain: String::new(),
            event,
        }],
    };

    let mut accepted = 0usize;
    for snap in snapshots {
        if !snap.domain.is_empty() && snap.domain != ".claude.ai" {
            continue;
        }
        if snap.cookies.is_empty() {
            continue;
        }
        let _ = s
            .tx
            .send(CookieEvent {
                profile_label: snap.profile_label,
                event: snap.event,
                cookies: snap.cookies,
            })
            .await;
        accepted += 1;
    }

    let body = Json(serde_json::json!({"ok": true, "accepted": accepted}));
    cors(body.into_response())
}

fn cors(mut resp: axum::response::Response) -> axum::response::Response {
    let h = resp.headers_mut();
    h.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    h.insert(
        "Access-Control-Allow-Methods",
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        "Access-Control-Allow-Headers",
        HeaderValue::from_static(
            "Content-Type, X-CB-Timestamp, X-CB-Nonce, X-CB-Signature",
        ),
    );
    resp
}

// ───────────────────────────── HMAC verification ────────────────────────────

fn verify_hmac(
    secret: &str,
    ts: &str,
    nonce: &str,
    method: &str,
    path: &str,
    body: &[u8],
    signature: &str,
) -> bool {
    if !signature.to_ascii_lowercase().starts_with("sha256=") {
        return false;
    }
    let provided = match hex::decode(&signature["sha256=".len()..]) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let ts_int: i64 = match ts.parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if (now - ts_int).abs() > TS_TOLERANCE_SEC {
        return false;
    }
    if nonce.len() < 8 {
        return false;
    }

    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(ts.as_bytes());
    mac.update(b"\n");
    mac.update(nonce.as_bytes());
    mac.update(b"\n");
    mac.update(method.as_bytes());
    mac.update(b"\n");
    mac.update(path.as_bytes());
    mac.update(b"\n");
    mac.update(body);

    mac.verify_slice(&provided).is_ok()
}

// ───────────────────────────── registration ─────────────────────────────────

async fn ensure_registered(app_dir: &Path) -> Result<(String, String)> {
    let path = app_dir.join(SECRET_FILE);
    if path.exists() {
        match tokio::fs::read(&path).await {
            Ok(bytes) => match serde_json::from_slice::<StoredCredentials>(&bytes) {
                Ok(stored) if !stored.id.is_empty() && !stored.secret.is_empty() => {
                    return Ok((stored.id, stored.secret));
                }
                Ok(_) => {
                    tracing::warn!(?path, "stored credentials malformed — re-registering");
                }
                Err(e) => {
                    tracing::warn!(error = %e, ?path, "stored credentials unparseable — re-registering");
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, ?path, "stored credentials unreadable — re-registering");
            }
        }
    }

    tracing::info!("registering with CookieBridge Hub at {HUB_URL}");
    let receiver_url = format!("http://127.0.0.1:{BRIDGE_PORT}/cookies");
    let manifest = RegisterManifest {
        id: CONSUMER_ID,
        display_name: CONSUMER_NAME,
        domains: &[".claude.ai"],
        profiles: &["*"],
        receiver: ReceiverSpec { url: &receiver_url },
        schema_version: "1.0",
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .no_proxy()
        .build()?;
    let resp = client
        .post(format!("{HUB_URL}/register"))
        .json(&manifest)
        .send()
        .await
        .with_context(|| format!("POST {HUB_URL}/register"))?;
    let status = resp.status();
    let body_text = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("<read err: {e}>"));
    if !status.is_success() {
        anyhow::bail!("Hub /register {} — {}", status, body_text);
    }
    let parsed: RegisterResponse = serde_json::from_str(&body_text)
        .with_context(|| format!("parse register response: {body_text}"))?;
    let id = parsed.id.unwrap_or_default();
    let secret = parsed.secret.unwrap_or_default();
    if id.is_empty() || secret.is_empty() {
        anyhow::bail!("Hub /register returned empty id/secret: {body_text}");
    }

    let stored = StoredCredentials {
        id: id.clone(),
        secret: secret.clone(),
    };
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    tokio::fs::write(&path, serde_json::to_vec_pretty(&stored)?)
        .await
        .with_context(|| format!("write {}", path.display()))?;
    tracing::info!(id = %id, "registered, secret persisted");
    Ok((id, secret))
}

// ───────────────────────────── port capture ─────────────────────────────────

/// Per global CLAUDE.md: same-port replacement, never port+1.
async fn capture_port() -> Result<()> {
    if !is_port_busy() {
        return Ok(());
    }
    tracing::warn!(port = BRIDGE_PORT, "port busy — asking old instance to shut down");
    if let Err(e) = ask_remote_shutdown().await {
        tracing::warn!(error = %e, "remote /shutdown failed");
    }

    for _ in 0..50 {
        if !is_port_busy() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    tracing::warn!("graceful shutdown timed out — killing process holding the port");
    if let Err(e) = kill_process_on_port().await {
        tracing::error!(error = %e, "kill_process_on_port failed");
    }

    for _ in 0..30 {
        if !is_port_busy() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("port {BRIDGE_PORT} still busy after kill — refusing to start on a different port");
}

fn is_port_busy() -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], BRIDGE_PORT)),
        Duration::from_millis(500),
    )
    .is_ok()
}

async fn ask_remote_shutdown() -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()?;
    let _ = client
        .post(format!("http://127.0.0.1:{BRIDGE_PORT}/shutdown"))
        .send()
        .await?;
    Ok(())
}

#[cfg(windows)]
async fn kill_process_on_port() -> Result<()> {
    let out = tokio::process::Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let needle = format!(":{BRIDGE_PORT}");
    let mut pid: Option<u32> = None;
    for line in stdout.lines() {
        if !line.contains(&needle) || !line.contains("LISTENING") {
            continue;
        }
        if let Some(p) = line.split_whitespace().last() {
            if let Ok(n) = p.parse::<u32>() {
                pid = Some(n);
                break;
            }
        }
    }
    let pid = pid.ok_or_else(|| anyhow::anyhow!("no LISTENING owner found on :{BRIDGE_PORT}"))?;
    tracing::warn!(pid, "killing process on port");
    let _ = tokio::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output()
        .await?;
    Ok(())
}

#[cfg(not(windows))]
async fn kill_process_on_port() -> Result<()> {
    anyhow::bail!("kill_process_on_port only implemented on Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, ts: &str, nonce: &str, method: &str, path: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(ts.as_bytes());
        mac.update(b"\n");
        mac.update(nonce.as_bytes());
        mac.update(b"\n");
        mac.update(method.as_bytes());
        mac.update(b"\n");
        mac.update(path.as_bytes());
        mac.update(b"\n");
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn verify_hmac_accepts_valid_signature() {
        let secret = "test-secret-abc";
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let nonce = "this-is-a-long-nonce";
        let body = br#"{"snapshots":[]}"#;
        let sig = sign(secret, &now, nonce, "POST", "/cookies", body);
        assert!(verify_hmac(secret, &now, nonce, "POST", "/cookies", body, &sig));
    }

    #[test]
    fn verify_hmac_rejects_tampered_body() {
        let secret = "test-secret-abc";
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let nonce = "this-is-a-long-nonce";
        let body = br#"{"snapshots":[]}"#;
        let sig = sign(secret, &now, nonce, "POST", "/cookies", body);
        let tampered = br#"{"snapshots":[{}]}"#;
        assert!(!verify_hmac(secret, &now, nonce, "POST", "/cookies", tampered, &sig));
    }

    #[test]
    fn verify_hmac_rejects_stale_timestamp() {
        let secret = "test-secret-abc";
        let stale = "100"; // way in the past
        let nonce = "this-is-a-long-nonce";
        let body = b"";
        let sig = sign(secret, stale, nonce, "POST", "/cookies", body);
        assert!(!verify_hmac(secret, stale, nonce, "POST", "/cookies", body, &sig));
    }

    #[test]
    fn verify_hmac_rejects_short_nonce() {
        let secret = "test-secret-abc";
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let body = b"";
        let sig = sign(secret, &now, "abc", "POST", "/cookies", body);
        assert!(!verify_hmac(secret, &now, "abc", "POST", "/cookies", body, &sig));
    }

    #[tokio::test]
    async fn parses_snapshots_envelope() {
        let body = serde_json::json!({
            "snapshots": [
                {
                    "profileLabel": "Default",
                    "domain": ".claude.ai",
                    "cookies": [{"name": "sessionKey", "value": "sk-x"}]
                }
            ],
            "serverTs": 1714430000
        });
        let parsed: PushBody = serde_json::from_value(body).unwrap();
        match parsed {
            PushBody::Snapshots { snapshots } => {
                assert_eq!(snapshots.len(), 1);
                assert_eq!(snapshots[0].cookies.len(), 1);
                assert_eq!(snapshots[0].profile_label, "Default");
            }
            _ => panic!("expected Snapshots variant"),
        }
    }

    #[tokio::test]
    async fn parses_bare_envelope() {
        let body = serde_json::json!({
            "cookies": [{"name": "x", "value": "y"}],
            "profileLabel": "Default"
        });
        let parsed: PushBody = serde_json::from_value(body).unwrap();
        match parsed {
            PushBody::Bare { cookies, profile_label, .. } => {
                assert_eq!(cookies.len(), 1);
                assert_eq!(profile_label, "Default");
            }
            _ => panic!("expected Bare variant"),
        }
    }
}
