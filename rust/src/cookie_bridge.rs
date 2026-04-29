//! HTTP receiver on `localhost:19225` for cookie pushes from CookieBridge.
//!
//! Routes:
//!   GET  /health                  → {"ok":true,"app":"ClaudeMonitor"}
//!   POST /cookies                 → CookieBridge v2 envelope
//!   POST /site-cookies/claude     → legacy envelope (v1)
//!   POST /shutdown                → graceful self-shutdown (used by port-capture)
//!
//! Port-capture (per global CLAUDE.md):
//! before binding we ping `:19225/shutdown` to ask the previous instance to
//! exit, then wait up to 5s. If still occupied → find PID via netstat and
//! `taskkill /PID N /F`. Same-port replacement is mandatory — no fallback to
//! port+1.

use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{HeaderValue, Method, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::types::{Cookie, CookieBridgePush, LegacyCookiePush};

pub const BRIDGE_PORT: u16 = 19225;

#[derive(Clone)]
struct AppState {
    tx: mpsc::Sender<CookieEvent>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

#[derive(Debug, Clone)]
pub struct CookieEvent {
    /// Profile label from CookieBridge v2 (browser profile name), empty for legacy.
    pub profile_label: String,
    pub event: String,
    pub cookies: Vec<Cookie>,
}

/// Bind to 127.0.0.1:19225 and serve until shutdown is requested.
/// Cookie events are forwarded over `tx`.
pub async fn run(tx: mpsc::Sender<CookieEvent>) -> Result<()> {
    capture_port().await?;

    let (sd_tx, mut sd_rx) = tokio::sync::watch::channel(false);
    let state = AppState { tx, shutdown_tx: sd_tx };

    let app = Router::new()
        .route("/health", get(health))
        .route("/cookies", post(post_v2).options(cors_preflight))
        .route("/site-cookies/claude", post(post_legacy).options(cors_preflight))
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

// ─────────────────────────────── handlers ────────────────────────────────────

async fn health() -> impl IntoResponse {
    let body = Json(serde_json::json!({"ok": true, "app": "ClaudeMonitor"}));
    cors(body.into_response())
}

async fn cors_preflight() -> impl IntoResponse {
    cors(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AnyPush {
    V2(CookieBridgePush),
    Legacy(LegacyCookiePush),
}

async fn post_v2(
    State(s): State<AppState>,
    Json(body): Json<CookieBridgePush>,
) -> impl IntoResponse {
    handle_push(&s, body.profile_label, body.event, body.cookies).await
}

async fn post_legacy(
    State(s): State<AppState>,
    Json(body): Json<LegacyCookiePush>,
) -> impl IntoResponse {
    handle_push(&s, String::new(), String::new(), body.cookies).await
}

async fn handle_push(
    state: &AppState,
    profile_label: String,
    event: String,
    cookies: Vec<Cookie>,
) -> axum::response::Response {
    let n = cookies.len();
    if n > 0 {
        let _ = state
            .tx
            .send(CookieEvent { profile_label, event, cookies })
            .await;
    }
    let body = Json(serde_json::json!({"ok": true, "count": n}));
    cors(body.into_response())
}

async fn post_shutdown(State(s): State<AppState>) -> impl IntoResponse {
    tracing::info!("/shutdown requested by remote");
    let _ = s.shutdown_tx.send(true);
    cors(Json(serde_json::json!({"ok": true})).into_response())
}

fn cors(mut resp: axum::response::Response) -> axum::response::Response {
    let h = resp.headers_mut();
    h.insert(
        "Access-Control-Allow-Origin",
        HeaderValue::from_static("*"),
    );
    h.insert(
        "Access-Control-Allow-Methods",
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        "Access-Control-Allow-Headers",
        HeaderValue::from_static("Content-Type"),
    );
    resp
}

// ─────────────────────────────── port capture ────────────────────────────────

/// Per global CLAUDE.md: same-port replacement, never port+1.
async fn capture_port() -> Result<()> {
    if !is_port_busy() {
        return Ok(());
    }
    tracing::warn!(port = BRIDGE_PORT, "port busy — asking old instance to shut down");
    if let Err(e) = ask_remote_shutdown().await {
        tracing::warn!(error = %e, "remote /shutdown failed");
    }

    // Wait up to 5s for graceful shutdown.
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

    // Final wait.
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
    // `netstat -ano -p TCP` → find the PID listening on our port → taskkill /F /PID.
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

    #[tokio::test]
    async fn parses_v2_envelope() {
        let json = r#"{
            "schemaVersion": 2,
            "timestamp": 1714430000,
            "profileLabel": "Default",
            "domain": "claude.ai",
            "event": "post-login",
            "cookies": [
                {"name": "sessionKey", "value": "sk-x", "domain": ".claude.ai"}
            ]
        }"#;
        let body: CookieBridgePush = serde_json::from_str(json).unwrap();
        assert_eq!(body.profile_label, "Default");
        assert_eq!(body.cookies.len(), 1);
        assert_eq!(body.cookies[0].name, "sessionKey");
    }

    #[tokio::test]
    async fn parses_legacy_envelope() {
        let json = r#"{"cookies": [{"name": "x", "value": "y", "domain": ".claude.ai"}]}"#;
        let body: LegacyCookiePush = serde_json::from_str(json).unwrap();
        assert_eq!(body.cookies.len(), 1);
    }
}
