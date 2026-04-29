// Claude Monitor — Rust port entry point.
//
// Threading model:
//   * Main thread: Slint event loop + tray-icon (winit/Win32 require this).
//   * Tokio multi-thread runtime: HTTP server, periodic fetchers, IPC.
//   * Background → UI updates via `slint::invoke_from_event_loop`.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use claude_monitor::{
    account_manager::AccountManager,
    api, claude_code_creds,
    cookie_bridge::{self, CookieEvent},
    http_client, paths,
    tray as tray_mod,
    trayconsole::{self, TrayCommand},
    types::Cookie,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

slint::include_modules!();

const FETCH_INTERVAL_SECS: u64 = 180;
const INCIDENTS_INTERVAL_SECS: u64 = 120;
const TRAYCONSOLE_PIPE: &str = "trayconsole_claude_monitor";

fn main() -> Result<()> {
    init_tracing();
    tracing::info!("ClaudeMonitor (Rust) starting");

    let app_dir = paths::app_dir();
    tracing::info!(?app_dir, "resolved app dir");

    let am = Arc::new(AccountManager::new(&app_dir)?);
    am.migrate_legacy().ok();
    claude_code_creds::backfill_from_disk(&am).ok();

    // ── Tokio runtime in a dedicated thread ──────────────────────────────
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()?;
    let rt_handle = rt.handle().clone();

    // ── Build UI ─────────────────────────────────────────────────────────
    let ui = AppWindow::new()?;
    let ui_weak = ui.as_weak();
    push_initial_state(&ui, &am);

    // ── Channels ─────────────────────────────────────────────────────────
    let (cookie_tx, cookie_rx) = mpsc::channel::<CookieEvent>(32);
    let (tray_cmd_tx, tray_cmd_rx) = mpsc::channel::<TrayCommand>(32);
    let (refresh_tx, refresh_rx) = mpsc::channel::<()>(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // ── Spawn background tasks ───────────────────────────────────────────
    {
        let am = am.clone();
        let app_dir = app_dir.clone();
        let ui_weak = ui_weak.clone();
        rt_handle.spawn(async move {
            handle_cookie_events(cookie_rx, am, app_dir, ui_weak).await;
        });
    }
    rt_handle.spawn({
        let tx = cookie_tx.clone();
        async move {
            if let Err(e) = cookie_bridge::run(tx).await {
                tracing::error!(error = %e, "cookie_bridge fatal");
            }
        }
    });
    rt_handle.spawn({
        let pipe = TRAYCONSOLE_PIPE.to_string();
        let tx = tray_cmd_tx.clone();
        let sd = shutdown_rx.clone();
        async move {
            if let Err(e) = trayconsole::run(pipe, tx, sd).await {
                tracing::error!(error = %e, "trayconsole fatal");
            }
        }
    });
    let refresh_tx_for_tray = refresh_tx.clone();
    {
        let am = am.clone();
        let app_dir = app_dir.clone();
        let ui_weak = ui_weak.clone();
        let mut sd = shutdown_rx.clone();
        rt_handle.spawn(async move {
            periodic_fetcher(am, app_dir, ui_weak, refresh_rx, &mut sd).await;
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let mut sd = shutdown_rx.clone();
        rt_handle.spawn(async move {
            periodic_incidents(ui_weak, &mut sd).await;
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let am = am.clone();
        let app_dir = app_dir.clone();
        rt_handle.spawn(async move {
            handle_tray_commands(tray_cmd_rx, ui_weak, am, app_dir, refresh_tx_for_tray).await;
        });
    }

    // ── Tray icon (must live for the lifetime of the app) ────────────────
    let _tray_handle = match tray_mod::build(None) {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!(error = %e, "tray icon failed (running without tray)");
            None
        }
    };

    // ── UI callbacks ─────────────────────────────────────────────────────
    {
        let ui_weak = ui_weak.clone();
        ui.on_close_clicked(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.window().hide().ok();
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        ui.on_minimize_clicked(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.window().hide().ok();
            }
        });
    }
    {
        let am = am.clone();
        let ui_weak = ui_weak.clone();
        let rt_handle = rt_handle.clone();
        ui.on_row_activated(move |id: SharedString| {
            let id = id.to_string();
            let am = am.clone();
            let ui_weak = ui_weak.clone();
            tracing::info!(account = %id, "switch account requested");
            if let Err(e) = am.switch_to(&id) {
                tracing::error!(error = %e, "switch_to failed");
            }
            claude_code_creds::sync_active_account(&am).ok();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| refresh_account_rows(&ui, &am));
        });
    }
    let _ = rt_handle; // keep handle alive (no-op binding to suppress unused warnings is implicit)
    {
        let am = am.clone();
        ui.on_add_account_clicked(move || {
            am.unblock_next_save(60);
            // Spawn the user's default browser at claude.ai/login. Profile
            // selection is a follow-up; this is good enough to test the
            // CookieBridge → AccountManager pipe.
            let _ = open_url("https://claude.ai/login");
        });
    }
    {
        let tx = refresh_tx.clone();
        let rt_handle = rt_handle.clone();
        ui.on_refresh_clicked(move || {
            let tx = tx.clone();
            rt_handle.spawn(async move { let _ = tx.send(()).await; });
        });
    }
    ui.on_toggle_auto_switch(|| {
        // Wired up but auto-switch logic is a follow-up (Task #12 stretch).
        tracing::info!("auto-switch toggle clicked");
    });

    // Trigger initial refresh.
    {
        let tx = refresh_tx.clone();
        let rt_handle = rt_handle.clone();
        rt_handle.spawn(async move { let _ = tx.send(()).await; });
    }

    // ── Run UI loop. Block here until window closes. ─────────────────────
    let run_result = ui.run();

    // Tear down background tasks.
    let _ = shutdown_tx.send(true);
    drop(rt);

    run_result.map_err(Into::into)
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .try_init();
}

// ─────────────────────────── UI helpers ──────────────────────────────────────

fn empty_metric(label: &str) -> MetricView {
    MetricView {
        label: label.into(),
        pct: 0.0,
        pct_text: "—".into(),
        reset_text: "".into(),
        color: slint::Color::from_rgb_u8(0x66, 0x66, 0x66),
        has_data: false,
    }
}

fn build_metric(label: &str, m: &claude_monitor::types::MetricBucket) -> MetricView {
    let pct = m.percent();
    let color = status_color(pct);
    MetricView {
        label: label.into(),
        pct: pct as f32,
        pct_text: format!("{:.0}%", pct).into(),
        reset_text: format_reset(m.resets_at.as_deref()).into(),
        color,
        has_data: true,
    }
}

fn status_color(pct: f64) -> slint::Color {
    if pct < 70.0 {
        slint::Color::from_rgb_u8(0x4a, 0xde, 0x80)
    } else if pct < 90.0 {
        slint::Color::from_rgb_u8(0xfa, 0xcc, 0x15)
    } else {
        slint::Color::from_rgb_u8(0xf8, 0x71, 0x71)
    }
}

fn format_reset(iso: Option<&str>) -> String {
    let Some(s) = iso else { return "—".into() };
    let Ok(when) = s.parse::<DateTime<Utc>>() else { return "—".into() };
    let now = Utc::now();
    let delta = when - now;
    if delta.num_seconds() <= 0 {
        return "сейчас".into();
    }
    let total_min = delta.num_minutes();
    if total_min < 60 {
        return format!("через {}м", total_min);
    }
    let h = total_min / 60;
    let m = total_min % 60;
    if h < 24 {
        if m == 0 {
            format!("через {}ч", h)
        } else {
            format!("через {}ч {}м", h, m)
        }
    } else {
        format!("через {}д {}ч", h / 24, h % 24)
    }
}

fn truncate6(s: &str) -> String {
    let mut chars: Vec<char> = s.chars().collect();
    if chars.len() <= 6 {
        return s.to_string();
    }
    chars.truncate(6);
    let s: String = chars.into_iter().collect();
    format!("{}…", s)
}

fn push_initial_state(ui: &AppWindow, am: &AccountManager) {
    ui.set_top_five_hour(empty_metric("5ч сессия"));
    ui.set_top_seven_day(empty_metric("7д лимит"));
    ui.set_top_seven_day_design(empty_metric("7д Design"));
    ui.set_last_update_text("—".into());
    refresh_account_rows(ui, am);
}

fn refresh_account_rows(ui: &AppWindow, am: &AccountManager) {
    let active_id = am.active_id().unwrap_or_default();
    let rows: Vec<AccountView> = am
        .all()
        .into_iter()
        .map(|a| AccountView {
            id: a.id.clone().into(),
            email: truncate6(&a.email).into(),
            email_full: a.email.clone().into(),
            plan: a.plan.into(),
            five_hour: empty_metric(""),
            seven_day: empty_metric(""),
            seven_day_design: empty_metric(""),
            is_active: a.id == active_id,
        })
        .collect();
    ui.set_accounts(ModelRc::new(VecModel::from(rows)));
}

// ─────────────────────────── background tasks ────────────────────────────────

async fn handle_cookie_events(
    mut rx: mpsc::Receiver<CookieEvent>,
    am: Arc<AccountManager>,
    app_dir: std::path::PathBuf,
    ui_weak: Weak<AppWindow>,
) {
    while let Some(ev) = rx.recv().await {
        let claude_cookies: Vec<Cookie> = ev
            .cookies
            .into_iter()
            .filter(|c| c.domain.contains("claude.ai"))
            .collect();
        if claude_cookies.is_empty() {
            continue;
        }
        let res = match am.save_cookies(claude_cookies) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "save_cookies failed");
                continue;
            }
        };
        let outcome = res.outcome;
        let Some(aid) = res.account_id else { continue };
        tracing::info!(?outcome, account = %aid, "cookies saved");

        // Try to resolve identity for fresh accounts.
        let cookie_file = am.account_file(&aid);
        if let Ok(ctx) = http_client::load_account_session(&cookie_file) {
            if let Ok(id) = api::fetch_identity(&app_dir, &ctx).await {
                am.update_info(&aid, Some(&id.email), Some(&id.display_name), Some(&id.plan), Some(&id.uuid)).ok();
                am.confirm(&aid).ok();
                tracing::info!(account = %aid, email = %id.email, "identity resolved");
            }
        }
        let am = am.clone();
        let _ = ui_weak.upgrade_in_event_loop(move |ui| refresh_account_rows(&ui, &am));
    }
}

async fn handle_tray_commands(
    mut rx: mpsc::Receiver<TrayCommand>,
    ui_weak: Weak<AppWindow>,
    am: Arc<AccountManager>,
    _app_dir: std::path::PathBuf,
    refresh_tx: mpsc::Sender<()>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            TrayCommand::Show => {
                let _ = ui_weak.upgrade_in_event_loop(|ui| { let _ = ui.window().show(); });
            }
            TrayCommand::Hide => {
                let _ = ui_weak.upgrade_in_event_loop(|ui| { let _ = ui.window().hide(); });
            }
            TrayCommand::Refresh => {
                let _ = refresh_tx.send(()).await;
            }
            TrayCommand::Relogin => {
                am.unblock_next_save(60);
                let _ = open_url("https://claude.ai/login");
            }
            TrayCommand::Stop => {
                let _ = ui_weak.upgrade_in_event_loop(|_ui| {
                    slint::quit_event_loop().ok();
                });
            }
            TrayCommand::Custom(name) => {
                tracing::info!(%name, "ignoring unknown tray command");
            }
        }
    }
}

async fn periodic_fetcher(
    am: Arc<AccountManager>,
    app_dir: std::path::PathBuf,
    ui_weak: Weak<AppWindow>,
    mut refresh_rx: mpsc::Receiver<()>,
    shutdown: &mut watch::Receiver<bool>,
) {
    let mut last_fetch = Instant::now() - Duration::from_secs(FETCH_INTERVAL_SECS);
    loop {
        if *shutdown.borrow() {
            break;
        }
        let due = last_fetch.elapsed() >= Duration::from_secs(FETCH_INTERVAL_SECS);
        if !due {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(FETCH_INTERVAL_SECS).saturating_sub(last_fetch.elapsed())) => {}
                Some(_) = refresh_rx.recv() => {}
                _ = shutdown.changed() => break,
            }
        }
        last_fetch = Instant::now();

        let Some(aid) = am.active_id() else { continue };
        let cookie_file = am.account_file(&aid);
        let ctx = match http_client::load_account_session(&cookie_file) {
            Ok(c) if c.has_session() => c,
            _ => {
                let _ = ui_weak.upgrade_in_event_loop(|ui| {
                    ui.set_status(StatusView { text: "сессия отсутствует".into(), error: true, visible: true });
                });
                continue;
            }
        };
        match api::fetch_usage(&app_dir, &ctx).await {
            Ok(u) => {
                let now_text = chrono::Local::now().format("%H:%M:%S").to_string();
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_top_five_hour(
                        u.five_hour.as_ref()
                            .map(|m| build_metric("5ч сессия", m))
                            .unwrap_or_else(|| empty_metric("5ч сессия")),
                    );
                    ui.set_top_seven_day(
                        u.seven_day.as_ref()
                            .map(|m| build_metric("7д лимит", m))
                            .unwrap_or_else(|| empty_metric("7д лимит")),
                    );
                    ui.set_top_seven_day_design(
                        u.seven_day_omelette.as_ref()
                            .map(|m| build_metric("7д Design", m))
                            .unwrap_or_else(|| empty_metric("7д Design")),
                    );
                    ui.set_last_update_text(now_text.into());
                    ui.set_status(StatusView { text: "".into(), error: false, visible: false });
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "fetch_usage failed");
                let txt = format!("ошибка fetch: {e}");
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_status(StatusView { text: txt.into(), error: true, visible: true });
                });
            }
        }
    }
}

async fn periodic_incidents(_ui_weak: Weak<AppWindow>, shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            break;
        }
        match api::fetch_incidents().await {
            Ok(list) => tracing::debug!(count = list.len(), "incidents"),
            Err(e) => tracing::debug!(error = %e, "incidents fetch failed"),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(INCIDENTS_INTERVAL_SECS)) => {}
            _ = shutdown.changed() => break,
        }
    }
}

#[cfg(windows)]
fn open_url(url: &str) -> std::io::Result<()> {
    std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn().map(|_| ())
}

#[cfg(not(windows))]
fn open_url(_url: &str) -> std::io::Result<()> {
    Ok(())
}
