// Claude Monitor — Rust port entry point.
//
// Threading model:
//   * Main thread: Slint event loop + tray-icon (winit/Win32 require this).
//   * Tokio multi-thread runtime: HTTP server, periodic fetchers, IPC.
//   * Background → UI updates via `slint::invoke_from_event_loop`.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use claude_monitor::{
    account_manager::AccountManager,
    api, claude_code_creds,
    cookie_bridge::{self, CookieEvent},
    http_client, paths, proxy,
    tray as tray_mod,
    trayconsole::{self, TrayCommand},
    types::{Cookie, MetricBucket, UsageResponse},
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};
use tracing_subscriber::EnvFilter;

slint::include_modules!();

/// Saved window size from full-mode, restored when leaving compact-mode.
/// Filled lazily on first compact entry.
static FULL_WINDOW_SIZE: parking_lot::Mutex<Option<(u32, u32)>> = parking_lot::Mutex::new(None);

/// Anti-phantom-click guard. Win32 `start_window_drag` (WM_SYSCOMMAND/SC_MOVE)
/// runs a modal pump that swallows mouse events; when it returns, Slint's
/// TouchArea sees press_pos == release_pos in window-local coords and fires
/// `clicked`, which would toggle compact-mode right after a drag. Compact
/// enter/exit handlers ignore clicks within 250 ms of a drag finishing.
static LAST_DRAG_END_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn recently_dragged() -> bool {
    let last = LAST_DRAG_END_MS.load(std::sync::atomic::Ordering::SeqCst);
    last != 0 && (now_ms() - last) < 250
}

const FETCH_INTERVAL_SECS: u64 = 180;
const PER_ACCOUNT_FETCH_INTERVAL_SECS: u64 = 60;
const INCIDENTS_INTERVAL_SECS: u64 = 120;
const TICK_INTERVAL_MS: u64 = 1000;
const TRAYCONSOLE_PIPE: &str = "trayconsole_claude_monitor";

/// Per-account cached usage so the UI can rebuild rows from a single source.
type UsageCache = Arc<AsyncMutex<HashMap<String, UsageResponse>>>;

fn main() -> Result<()> {
    init_tracing();
    tracing::info!("ClaudeMonitor (Rust) starting");

    let app_dir = paths::app_dir();
    tracing::info!(?app_dir, "resolved app dir");

    let am = Arc::new(AccountManager::new(&app_dir)?);
    am.migrate_legacy().ok();
    claude_code_creds::backfill_from_disk(&am).ok();

    let usage_cache: UsageCache = Arc::new(AsyncMutex::new(HashMap::new()));

    // ── Tokio runtime in a dedicated thread ──────────────────────────────
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()?;
    let rt_handle = rt.handle().clone();

    // ── Build UI ─────────────────────────────────────────────────────────
    let ui = AppWindow::new()?;
    let ui_weak = ui.as_weak();
    push_initial_state(&ui, &am, &app_dir);

    // ── Channels ─────────────────────────────────────────────────────────
    let (cookie_tx, cookie_rx) = mpsc::channel::<CookieEvent>(32);
    let (tray_cmd_tx, tray_cmd_rx) = mpsc::channel::<TrayCommand>(32);
    let (refresh_tx, refresh_rx) = mpsc::channel::<RefreshKind>(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Side-by-side mode: when running alongside the Python ClaudeMonitor,
    // CMON_DISABLE_BRIDGE=1 and CMON_DISABLE_TRAYCONSOLE=1 stop us from
    // fighting over :19225 and the trayconsole_claude_monitor pipe.
    let bridge_disabled = std::env::var("CMON_DISABLE_BRIDGE").is_ok();
    let trayconsole_disabled = std::env::var("CMON_DISABLE_TRAYCONSOLE").is_ok();
    tracing::info!(bridge_disabled, trayconsole_disabled, "side-by-side mode flags");

    // ── Spawn background tasks ───────────────────────────────────────────
    {
        let am = am.clone();
        let app_dir = app_dir.clone();
        let ui_weak = ui_weak.clone();
        let usage_cache = usage_cache.clone();
        rt_handle.spawn(async move {
            handle_cookie_events(cookie_rx, am, app_dir, ui_weak, usage_cache).await;
        });
    }
    if !bridge_disabled {
        rt_handle.spawn({
            let tx = cookie_tx.clone();
            let app_dir = app_dir.clone();
            async move {
                if let Err(e) = cookie_bridge::run(tx, app_dir).await {
                    tracing::error!(error = %e, "cookie_bridge fatal");
                }
            }
        });
    }
    if !trayconsole_disabled {
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
    }
    // Tray handle reference for periodic_fetcher (so it can update icons).
    // tray-icon's TrayIcon is !Send/!Sync because of HWND, but set_icon is
    // implemented via Windows messages (thread-safe in practice). We wrap
    // it manually so we can pass it across tokio tasks.
    let tray_for_fetcher: Arc<parking_lot::Mutex<Option<SendableTray>>> =
        Arc::new(parking_lot::Mutex::new(None));
    {
        let am = am.clone();
        let app_dir = app_dir.clone();
        let ui_weak = ui_weak.clone();
        let usage_cache = usage_cache.clone();
        let tray_for_fetcher = tray_for_fetcher.clone();
        let mut sd = shutdown_rx.clone();
        rt_handle.spawn(async move {
            periodic_fetcher(am, app_dir, ui_weak, usage_cache, tray_for_fetcher, refresh_rx, &mut sd).await;
        });
    }
    {
        let mut sd = shutdown_rx.clone();
        rt_handle.spawn(async move {
            periodic_incidents(&mut sd).await;
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let am = am.clone();
        let refresh_tx = refresh_tx.clone();
        rt_handle.spawn(async move {
            handle_tray_commands(tray_cmd_rx, ui_weak, am, refresh_tx).await;
        });
    }
    // Live countdown ticker — recomputes "Хч Yм" labels every second.
    {
        let ui_weak = ui_weak.clone();
        let am = am.clone();
        let usage_cache = usage_cache.clone();
        let mut sd = shutdown_rx.clone();
        rt_handle.spawn(async move {
            loop {
                if *sd.borrow() { break; }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(TICK_INTERVAL_MS)) => {}
                    _ = sd.changed() => break,
                }
                let cache = usage_cache.lock().await.clone();
                let am = am.clone();
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    refresh_account_rows(&ui, &am, &cache);
                });
            }
        });
    }
    let _ = cookie_tx; // sender lives until drop; rx in handle_cookie_events
    let _ = tray_cmd_tx;

    // ── Tray icons (session + weekly) ────────────────────────────────────
    let _tray_handle = match tray_mod::build(None, None) {
        Ok(h) => {
            let arc = Arc::new(h);
            wire_tray_events(&arc, ui_weak.clone(), am.clone());
            *tray_for_fetcher.lock() = Some(SendableTray(arc.clone()));
            Some(arc)
        }
        Err(e) => {
            tracing::warn!(error = %e, "tray icon failed (running without tray)");
            None
        }
    };

    // ── UI callbacks ─────────────────────────────────────────────────────
    let app_dir_ui = app_dir.clone();

    ui.on_close_clicked(move || {
        tracing::info!("close-clicked → quit_event_loop");
        let _ = slint::quit_event_loop();
    });
    {
        let ui_weak = ui_weak.clone();
        ui.on_minimize_clicked(move || {
            tracing::info!("minimize-clicked → window.hide() (restore via tray icon)");
            if let Some(ui) = ui_weak.upgrade() {
                let _ = ui.window().hide();
            }
        });
    }
    ui.on_quit_clicked(|| {
        tracing::info!("quit-clicked → quit_event_loop");
        let _ = slint::quit_event_loop();
    });
    {
        let ui_weak = ui_weak.clone();
        ui.on_header_clicked(move || {
            // Suppress phantom click that fires immediately after a drag —
            // Slint sees release at the same window-local pos as press because
            // the OS moved the whole window with the cursor.
            if recently_dragged() {
                return;
            }
            tracing::info!("header-clicked → compact-mode=true");
            if let Some(ui) = ui_weak.upgrade() {
                // Remember the full-mode size so compact-exit can restore it.
                let sz = ui.window().size();
                *FULL_WINDOW_SIZE.lock() = Some((sz.width, sz.height));
                ui.set_compact_mode(true);
                ui.window().set_size(slint::LogicalSize::new(160.0, 36.0));
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        ui.on_compact_exit_clicked(move || {
            if recently_dragged() {
                return;
            }
            tracing::info!("compact-exit → compact-mode=false");
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_compact_mode(false);
                let restore = FULL_WINDOW_SIZE
                    .lock()
                    .unwrap_or((380, 240));
                ui.window()
                    .set_size(slint::PhysicalSize::new(restore.0, restore.1));
            }
        });
    }
    ui.on_open_claude_status(|| { let _ = open_url("https://status.claude.com"); });
    {
        let am = am.clone();
        let ui_weak = ui_weak.clone();
        ui.on_relogin_clicked(move || {
            am.unblock_next_save(60);
            let _ = open_url("https://claude.ai/login");
            // Update status on UI.
            let _ = ui_weak.upgrade_in_event_loop(|ui| {
                ui.set_status(StatusView { text: "ожидание куков…".into(), error: false });
            });
        });
    }
    {
        let app_dir = app_dir_ui.clone();
        let ui_weak = ui_weak.clone();
        let rt_handle_inner = rt_handle.clone();
        ui.on_configure_proxy_clicked(move || {
            let app_dir = app_dir.clone();
            let ui_weak = ui_weak.clone();
            rt_handle_inner.spawn(async move {
                let current = proxy::load_proxy_url(&app_dir);
                let prompt = "URL прокси (пусто — без прокси):";
                let new = match show_input_dialog("Настройка прокси", prompt, &current).await {
                    Some(s) => s,
                    None => return,
                };
                if let Err(e) = proxy::save_proxy_url(&app_dir, &new) {
                    tracing::warn!(error = %e, "save proxy failed");
                }
                let label = format_proxy_label(&proxy::load_proxy_url(&app_dir));
                let _ = ui_weak.upgrade_in_event_loop(move |ui| ui.set_proxy_label(label.into()));
            });
        });
    }
    {
        let am = am.clone();
        let ui_weak = ui_weak.clone();
        let usage_cache_ui = usage_cache.clone();
        ui.on_row_activated(move |id: SharedString| {
            let id = id.to_string();
            tracing::info!(account = %id, "switch account requested");
            if let Err(e) = am.switch_to(&id) {
                tracing::error!(error = %e, "switch_to failed");
            }
            claude_code_creds::sync_active_account(&am).ok();
            let am = am.clone();
            let cache = usage_cache_ui.clone();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                let snap = cache.try_lock().map(|g| g.clone()).unwrap_or_default();
                refresh_account_rows(&ui, &am, &snap);
            });
        });
    }
    {
        let am = am.clone();
        let ui_weak = ui_weak.clone();
        let usage_cache_ui = usage_cache.clone();
        ui.on_row_remove(move |id: SharedString| {
            let id = id.to_string();
            tracing::info!(account = %id, "remove account requested");
            if let Err(e) = am.remove(&id) {
                tracing::error!(error = %e, "remove failed");
            }
            let am = am.clone();
            let cache = usage_cache_ui.clone();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                let snap = cache.try_lock().map(|g| g.clone()).unwrap_or_default();
                refresh_account_rows(&ui, &am, &snap);
            });
        });
    }
    {
        let am = am.clone();
        ui.on_add_account_clicked(move || {
            am.unblock_next_save(60);
            let _ = open_url("https://claude.ai/login");
        });
    }
    {
        let tx = refresh_tx.clone();
        let rt_handle_inner = rt_handle.clone();
        ui.on_refresh_clicked(move || {
            tracing::info!("refresh-clicked");
            let tx = tx.clone();
            rt_handle_inner.spawn(async move { let _ = tx.send(RefreshKind::Active).await; });
        });
    }
    {
        let tx = refresh_tx.clone();
        let rt_handle_inner = rt_handle.clone();
        ui.on_refresh_all_clicked(move || {
            tracing::info!("refresh-all-clicked");
            let tx = tx.clone();
            rt_handle_inner.spawn(async move { let _ = tx.send(RefreshKind::All).await; });
        });
    }
    {
        let ui_weak = ui_weak.clone();
        ui.on_toggle_auto_switch(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let now = !ui.get_auto_switch_enabled();
                tracing::info!(?now, "auto-switch toggled");
                ui.set_auto_switch_enabled(now);
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        ui.on_drag_start(move || {
            if let Some(ui) = ui_weak.upgrade() {
                #[cfg(windows)]
                {
                    // Compare window position before/after the modal SC_MOVE
                    // pump so we only set the anti-phantom-click guard when a
                    // real drag happened. A pure tap (down+up no movement) must
                    // still allow the compact toggle to fire.
                    let pre = window_top_left(&ui);
                    start_window_drag(&ui);
                    let post = window_top_left(&ui);
                    let moved = match (pre, post) {
                        (Some(a), Some(b)) => (a.0 - b.0).abs() > 3 || (a.1 - b.1).abs() > 3,
                        _ => false,
                    };
                    if moved {
                        LAST_DRAG_END_MS.store(now_ms(), std::sync::atomic::Ordering::SeqCst);
                    }
                }
                #[cfg(not(windows))]
                let _ = ui;
            }
        });
    }

    // Initial status + refresh.
    {
        let label = format_proxy_label(&proxy::load_proxy_url(&app_dir));
        ui.set_proxy_label(label.into());
    }
    {
        let tx = refresh_tx.clone();
        rt_handle.spawn(async move { let _ = tx.send(RefreshKind::All).await; });
    }

    // Hide from the Windows taskbar / Alt-Tab list. WS_EX_TOOLWINDOW must be
    // applied AFTER the window has been realised; we schedule it on the next
    // event-loop tick so the HWND is definitely available.
    #[cfg(windows)]
    {
        let ui_weak = ui.as_weak();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                make_tool_window(&ui);
            }
        });
    }

    // ── Run UI loop. ────────────────────────────────────────────────────
    // Use run_event_loop_until_quit so hiding the last window (minimize → hide)
    // doesn't terminate the loop. Tray icon needs the loop alive to keep
    // receiving shell messages; explicit quit happens via close/quit/Stop.
    ui.show()?;
    let run_result = slint::run_event_loop_until_quit();
    let _ = ui.hide();

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

#[derive(Debug, Clone, Copy)]
enum RefreshKind { Active, All }

// ─────────────────────────── UI helpers ──────────────────────────────────────

fn empty_metric() -> MetricView {
    MetricView {
        pct: 0.0,
        pct_text: "".into(),
        reset_text: "".into(),
        color: slint::Color::from_rgb_u8(0x55, 0x55, 0x55),
        has_data: false,
    }
}

fn build_metric(m: &MetricBucket) -> MetricView {
    let pct = m.percent();
    MetricView {
        pct: pct as f32,
        pct_text: format!("{:.0}%", pct).into(),
        reset_text: format_remaining(m.resets_at.as_deref()).into(),
        color: status_color(pct),
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

/// Replicates Python `_fmt_remaining`: "Хч Yм" / "Хм" / "Хд Yч".
fn format_remaining(iso: Option<&str>) -> String {
    let Some(s) = iso else { return "".into() };
    let Ok(when) = s.parse::<DateTime<Utc>>() else { return "".into() };
    let now = Utc::now();
    let secs = (when - now).num_seconds();
    if secs <= 0 {
        return "сброс".into();
    }
    if secs < 86_400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        return if h > 0 { format!("{h}ч {m}м") } else { format!("{m}м") };
    }
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    format!("{d}д {h}ч")
}

fn login_display(email: &str, name: &str) -> String {
    // Pass the full local part — Slint clips visually inside the 80px column
    // (overflow: clip, no ellipsis), and the hover tooltip shows the full email.
    if !email.is_empty() {
        email.split('@').next().unwrap_or(email).to_string()
    } else if !name.is_empty() {
        name.to_string()
    } else {
        "…".into()
    }
}

fn plan_color(plan: &str) -> slint::Color {
    match plan {
        "Pro" => slint::Color::from_rgb_u8(0xa7, 0x8b, 0xfa),
        "Max" | "Max100" => slint::Color::from_rgb_u8(0x60, 0xa5, 0xfa),
        "Max200" => slint::Color::from_rgb_u8(0x38, 0xbd, 0xf8),
        "Free" => slint::Color::from_rgb_u8(0x6b, 0x72, 0x80),
        "Team" | "Enterprise" | "Ent" => slint::Color::from_rgb_u8(0xf5, 0x9e, 0x0b),
        _ => slint::Color::from_rgb_u8(0x6b, 0x72, 0x80),
    }
}

fn format_proxy_label(url: &str) -> String {
    if url.trim().is_empty() {
        "Прокси: (не задан)".into()
    } else {
        format!("Прокси (fallback): {url}")
    }
}

fn push_initial_state(ui: &AppWindow, am: &AccountManager, app_dir: &std::path::Path) {
    let label = format_proxy_label(&proxy::load_proxy_url(app_dir));
    ui.set_proxy_label(label.into());
    ui.set_status(StatusView { text: "загрузка…".into(), error: false });
    refresh_account_rows(ui, am, &HashMap::new());
}

fn build_account_view(
    a: &claude_monitor::types::Account,
    is_active: bool,
    cached: Option<&UsageResponse>,
) -> AccountView {
    let (five, seven, design) = match cached {
        Some(u) => (
            u.five_hour.as_ref().map(build_metric).unwrap_or_else(empty_metric),
            u.seven_day.as_ref().map(build_metric).unwrap_or_else(empty_metric),
            u.seven_day_omelette.as_ref().map(build_metric).unwrap_or_else(empty_metric),
        ),
        None => (empty_metric(), empty_metric(), empty_metric()),
    };
    AccountView {
        id: a.id.clone().into(),
        login: login_display(&a.email, &a.name).into(),
        login_full: a.email.clone().into(),
        plan: a.plan.clone().into(),
        plan_color: plan_color(&a.plan),
        five_hour: five,
        seven_day: seven,
        seven_day_design: design,
        is_active,
    }
}

/// Ranking key for accounts (1:1 port of Python `_account_sort_key`).
/// Tuple lex-sort: group → secs to 5h reset → -(session_pct).
/// Group: 1 = usable, 2 = session full, 3 = weekly full.
fn account_sort_key(usage: Option<&UsageResponse>) -> (u8, i64, i32) {
    let session_pct = usage
        .and_then(|u| u.five_hour.as_ref())
        .map(|m| m.percent())
        .unwrap_or(0.0);
    let weekly_pct = usage
        .and_then(|u| u.seven_day.as_ref())
        .map(|m| m.percent())
        .unwrap_or(0.0);
    let secs = usage
        .and_then(|u| u.five_hour.as_ref())
        .and_then(|m| m.resets_at.as_ref())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .map(|when| (when - Utc::now()).num_seconds().max(0))
        .unwrap_or(99_999);
    if weekly_pct >= 100.0 {
        (3, 99_999, 0)
    } else if session_pct >= 100.0 {
        (2, secs, 0)
    } else {
        (1, secs, -(session_pct as i32))
    }
}

fn refresh_account_rows(
    ui: &AppWindow,
    am: &AccountManager,
    usage: &HashMap<String, UsageResponse>,
) {
    let active_id = am.active_id().unwrap_or_default();
    let mut accounts = am.all();
    accounts.sort_by_key(|a| account_sort_key(usage.get(&a.id)));
    let rows: Vec<AccountView> = accounts
        .into_iter()
        .map(|a| build_account_view(&a, a.id == active_id, usage.get(&a.id)))
        .collect();

    // In-place update of the existing VecModel — preserves Slint's per-row
    // state (e.g. chk_touch hover), which would otherwise reset on each tick
    // because the live ticker calls this every second. Falls back to wholesale
    // replacement if the model isn't a VecModel for some reason.
    let model_rc = ui.get_accounts();
    if let Some(vec_model) = model_rc.as_any().downcast_ref::<VecModel<AccountView>>() {
        // Trim excess rows.
        while vec_model.row_count() > rows.len() {
            vec_model.remove(vec_model.row_count() - 1);
        }
        // Update existing + append new.
        for (i, row) in rows.iter().enumerate() {
            if i < vec_model.row_count() {
                vec_model.set_row_data(i, row.clone());
            } else {
                vec_model.push(row.clone());
            }
        }
    } else {
        ui.set_accounts(ModelRc::new(VecModel::from(rows.clone())));
    }

    // Compact label: "85% (1ч 23м)" for the active account's 5h session.
    // Falls back to "—" when no active account or when its 5h metric is empty.
    let compact = rows
        .iter()
        .find(|r| r.is_active && r.five_hour.has_data)
        .map(|r| {
            let pct = r.five_hour.pct_text.as_str();
            let reset = r.five_hour.reset_text.as_str();
            if reset.is_empty() {
                pct.to_string()
            } else {
                format!("{pct}  ({reset})")
            }
        })
        .unwrap_or_else(|| "—".to_string());
    ui.set_compact_text(compact.into());

    // Diagnostic dump (kept; cheap).
    let snapshot: Vec<_> = rows.iter().map(|r| serde_json::json!({
        "id": r.id.as_str(),
        "login": r.login.as_str(),
        "is_active": r.is_active,
        "five_hour":  { "has": r.five_hour.has_data,         "pct": r.five_hour.pct_text.as_str(),         "reset": r.five_hour.reset_text.as_str() },
        "seven_day":  { "has": r.seven_day.has_data,         "pct": r.seven_day.pct_text.as_str(),         "reset": r.seven_day.reset_text.as_str() },
        "design":     { "has": r.seven_day_design.has_data,  "pct": r.seven_day_design.pct_text.as_str(),  "reset": r.seven_day_design.reset_text.as_str() },
    })).collect();
    if let Ok(s) = serde_json::to_string_pretty(&snapshot) {
        let _ = std::fs::write(paths::app_dir().join("last_account_views.json"), s);
    }
}

// ─────────────────────────── background tasks ────────────────────────────────

async fn handle_cookie_events(
    mut rx: mpsc::Receiver<CookieEvent>,
    am: Arc<AccountManager>,
    app_dir: PathBuf,
    ui_weak: Weak<AppWindow>,
    usage_cache: UsageCache,
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

        let cookie_file = am.account_file(&aid);
        if let Ok(ctx) = http_client::load_account_session(&cookie_file) {
            if let Ok(id) = api::fetch_identity(&app_dir, &ctx).await {
                am.update_info(&aid, Some(&id.email), Some(&id.display_name), Some(&id.plan), Some(&id.uuid)).ok();
                am.confirm(&aid).ok();
                tracing::info!(account = %aid, email = %id.email, "identity resolved");
            }
        }
        let am = am.clone();
        let cache = usage_cache.lock().await.clone();
        let _ = ui_weak.upgrade_in_event_loop(move |ui| refresh_account_rows(&ui, &am, &cache));
    }
}

async fn handle_tray_commands(
    mut rx: mpsc::Receiver<TrayCommand>,
    ui_weak: Weak<AppWindow>,
    am: Arc<AccountManager>,
    refresh_tx: mpsc::Sender<RefreshKind>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            TrayCommand::Show => {
                let _ = ui_weak.upgrade_in_event_loop(|ui| {
                    let _ = ui.window().show();
                    #[cfg(windows)]
                    {
                        make_tool_window(&ui);
                        restore_and_focus(&ui);
                    }
                });
            }
            TrayCommand::Hide => {
                let _ = ui_weak.upgrade_in_event_loop(|ui| { let _ = ui.window().hide(); });
            }
            TrayCommand::Refresh => { let _ = refresh_tx.send(RefreshKind::All).await; }
            TrayCommand::Relogin => {
                am.unblock_next_save(60);
                let _ = open_url("https://claude.ai/login");
            }
            TrayCommand::Stop => {
                let _ = ui_weak.upgrade_in_event_loop(|_ui| {
                    let _ = slint::quit_event_loop();
                });
            }
            TrayCommand::Custom(name) => {
                tracing::info!(%name, "ignoring unknown tray command");
            }
        }
    }
}

/// Newtype wrapper that asserts Send+Sync for the !Send TrayHandle. Sound
/// because tray-icon's set_icon dispatches via Win32 messages (thread-safe).
#[derive(Clone)]
struct SendableTray(Arc<tray_mod::TrayHandle>);
unsafe impl Send for SendableTray {}
unsafe impl Sync for SendableTray {}

/// Drops `is-refreshing` back to `false` even if the surrounding future panics
/// or is cancelled mid-fetch. Without this, a stuck network request would leave
/// the spinner whirling forever and starve UI clicks.
struct RefreshGuard {
    ui: Weak<AppWindow>,
}
impl RefreshGuard {
    fn new(ui: Weak<AppWindow>) -> Self {
        let w = ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = w.upgrade() { ui.set_is_refreshing(true); }
        });
        Self { ui }
    }
}
impl Drop for RefreshGuard {
    fn drop(&mut self) {
        let w = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = w.upgrade() { ui.set_is_refreshing(false); }
        });
    }
}

const FETCH_TIMEOUT_SECS: u64 = 15;

async fn periodic_fetcher(
    am: Arc<AccountManager>,
    app_dir: PathBuf,
    ui_weak: Weak<AppWindow>,
    usage_cache: UsageCache,
    tray: Arc<parking_lot::Mutex<Option<SendableTray>>>,
    mut refresh_rx: mpsc::Receiver<RefreshKind>,
    shutdown: &mut watch::Receiver<bool>,
) {
    let mut last_active_fetch = Instant::now() - Duration::from_secs(FETCH_INTERVAL_SECS);
    let mut last_all_fetch = Instant::now() - Duration::from_secs(PER_ACCOUNT_FETCH_INTERVAL_SECS);

    loop {
        if *shutdown.borrow() { break; }

        let active_due = last_active_fetch.elapsed() >= Duration::from_secs(FETCH_INTERVAL_SECS);
        let all_due = last_all_fetch.elapsed() >= Duration::from_secs(PER_ACCOUNT_FETCH_INTERVAL_SECS);

        let mut requested: Option<RefreshKind> = None;
        if !active_due && !all_due {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(15)) => {}
                Some(k) = refresh_rx.recv() => { requested = Some(k); }
                _ = shutdown.changed() => break,
            }
        } else {
            // Drain any pending refresh request without blocking.
            if let Ok(k) = refresh_rx.try_recv() { requested = Some(k); }
        }

        let do_active = active_due || matches!(requested, Some(RefreshKind::Active) | Some(RefreshKind::All));
        let do_all    = all_due    || matches!(requested, Some(RefreshKind::All));

        let active_id = am.active_id().unwrap_or_default();

        // RAII guard: spinner is on for the whole iteration; it's reset by Drop
        // when this block exits (normal return, error, or panic).
        let _spin_guard = ((do_active || do_all) && !active_id.is_empty())
            .then(|| RefreshGuard::new(ui_weak.clone()));

        if do_active && !active_id.is_empty() {
            last_active_fetch = Instant::now();
            fetch_one(&am, &app_dir, &active_id, &usage_cache, &ui_weak).await;
            // Update tray icons from active account's usage.
            let tray_clone = tray.lock().clone();
            if let Some(t) = tray_clone {
                let cache = usage_cache.lock().await;
                if let Some(u) = cache.get(&active_id) {
                    let s = u.five_hour.as_ref().map(|m| m.percent());
                    let w = u.seven_day.as_ref().map(|m| m.percent());
                    let s_reset = format_remaining(
                        u.five_hour.as_ref().and_then(|m| m.resets_at.as_deref()),
                    );
                    let w_reset = format_remaining(
                        u.seven_day.as_ref().and_then(|m| m.resets_at.as_deref()),
                    );
                    let _ = tray_mod::update_session(&t.0, s, &s_reset);
                    let _ = tray_mod::update_weekly(&t.0, w, &w_reset);
                }
            }
        }
        if do_all {
            last_all_fetch = Instant::now();
            for acc in am.all() {
                if acc.id == active_id { continue; } // already done above
                fetch_one(&am, &app_dir, &acc.id, &usage_cache, &ui_weak).await;
            }
        }

        // _spin_guard drops here → spinner stops.
    }
}

async fn fetch_one(
    am: &Arc<AccountManager>,
    app_dir: &std::path::Path,
    aid: &str,
    usage_cache: &UsageCache,
    ui_weak: &Weak<AppWindow>,
) {
    let cookie_file = am.account_file(aid);
    let ctx = match http_client::load_account_session(&cookie_file) {
        Ok(c) if c.has_session() => c,
        _ => {
            let aid_owned = aid.to_string();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                ui.set_status(StatusView { text: format!("сессия {} отсутствует", aid_owned).into(), error: true });
            });
            return;
        }
    };
    let fut = api::fetch_usage(app_dir, &ctx);
    let result = match tokio::time::timeout(Duration::from_secs(FETCH_TIMEOUT_SECS), fut).await {
        Ok(r) => r,
        Err(_) => {
            tracing::warn!(account = %aid, "fetch_usage timed out");
            let aid_owned = aid.to_string();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                ui.set_status(StatusView { text: format!("таймаут fetch ({})", aid_owned).into(), error: true });
            });
            return;
        }
    };
    match result {
        Ok(u) => {
            {
                let mut g = usage_cache.lock().await;
                g.insert(aid.to_string(), u);
            }
            let am = am.clone();
            let cache = usage_cache.lock().await.clone();
            let now_text = chrono::Local::now().format("%H:%M:%S").to_string();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                refresh_account_rows(&ui, &am, &cache);
                ui.set_status(StatusView { text: format!("обновлено: {now_text}").into(), error: false });
            });
        }
        Err(e) => {
            tracing::warn!(account = %aid, error = %e, "fetch_usage failed");
            let aid_owned = aid.to_string();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                ui.set_status(StatusView { text: format!("ошибка fetch ({}): {}", aid_owned, e).into(), error: true });
            });
        }
    }
}

async fn periodic_incidents(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() { break; }
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

// ─────────────────────────── platform helpers ────────────────────────────────

#[cfg(windows)]
fn open_url(url: &str) -> std::io::Result<()> {
    std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn().map(|_| ())
}

#[cfg(not(windows))]
fn open_url(_url: &str) -> std::io::Result<()> { Ok(()) }

/// Wire tray-icon's MenuEvent + TrayIconEvent receivers to UI actions.
/// Each receiver runs in its own std::thread because both are global
/// crossbeam channels; they'd block each other in a single thread.
fn wire_tray_events(
    handle: &Arc<tray_mod::TrayHandle>,
    ui_weak: Weak<AppWindow>,
    am: Arc<AccountManager>,
) {
    let show_id = handle.show_id.0.clone();
    let add_id = handle.add_id.0.clone();
    let quit_id = handle.quit_id.0.clone();

    // Menu item events
    {
        let ui_weak = ui_weak.clone();
        let am = am.clone();
        std::thread::spawn(move || {
            let rx = tray_icon::menu::MenuEvent::receiver();
            while let Ok(ev) = rx.recv() {
                let id = ev.id().0.clone();
                tracing::info!(menu_id = %id, "tray menu event");
                if id == show_id {
                    let ui_w = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            let _ = ui.window().show();
                            #[cfg(windows)]
                            {
                                make_tool_window(&ui);
                                restore_and_focus(&ui);
                            }
                        }
                    });
                } else if id == add_id {
                    am.unblock_next_save(60);
                    let _ = open_url("https://claude.ai/login");
                } else if id == quit_id {
                    let _ = slint::invoke_from_event_loop(|| {
                        let _ = slint::quit_event_loop();
                    });
                    break;
                }
            }
        });
    }

    // Tray-icon click events (left-click → show)
    {
        let ui_weak = ui_weak.clone();
        std::thread::spawn(move || {
            use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};
            let rx = TrayIconEvent::receiver();
            while let Ok(ev) = rx.recv() {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                    tracing::info!("tray icon left-click → show window");
                    let ui_w = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            let _ = ui.window().show();
                            #[cfg(windows)]
                            {
                                make_tool_window(&ui);
                                restore_and_focus(&ui);
                            }
                        }
                    });
                }
            }
        });
    }
}

/// Win32 helper: extract HWND from Slint's window handle.
/// Win32 helper: read the current screen coordinates of the window's top-left
/// corner. Used by `on_drag_start` to detect whether SC_MOVE actually moved
/// the window (real drag) vs. a pure click (no movement) — the guard against
/// Slint's phantom `clicked` event must only arm in the former case.
#[cfg(windows)]
fn window_top_left(ui: &AppWindow) -> Option<(i32, i32)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let hwnd = slint_hwnd(ui)?;
    let mut r = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut r).ok()?; }
    Some((r.left, r.top))
}

#[cfg(windows)]
fn slint_hwnd(ui: &AppWindow) -> Option<windows::Win32::Foundation::HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    let win_handle = ui.window().window_handle();
    let raw = win_handle.window_handle().ok()?.as_raw();
    let RawWindowHandle::Win32(w) = raw else { return None };
    Some(HWND(w.hwnd.get() as *mut _))
}

#[cfg(windows)]
fn restore_and_focus(ui: &AppWindow) {
    use windows::Win32::UI::WindowsAndMessaging::{
        IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };
    if let Some(hwnd) = slint_hwnd(ui) {
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            } else {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// Toggle WS_EX_TOOLWINDOW so the window disappears from the taskbar and
/// Alt-Tab. WS_EX_APPWINDOW (often set by default) overrides TOOLWINDOW for
/// taskbar visibility, so we explicitly clear it. SetWindowPos with
/// SWP_FRAMECHANGED forces the shell to re-evaluate the window's category.
#[cfg(windows)]
fn make_tool_window(ui: &AppWindow) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_EXSTYLE,
        HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SW_HIDE, SW_SHOWNOACTIVATE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };
    let Some(hwnd) = slint_hwnd(ui) else { return };
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
        let mut ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        ex |=  WS_EX_TOOLWINDOW.0 as isize;
        ex &= !(WS_EX_APPWINDOW.0 as isize);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
        let _ = SetWindowPos(
            hwnd, HWND_TOPMOST, 0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

#[cfg(windows)]
fn start_window_drag(ui: &AppWindow) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    use windows::Win32::UI::WindowsAndMessaging::{
        PostMessageW, SendMessageW, HTCAPTION, WM_LBUTTONUP, WM_NCLBUTTONDOWN,
    };
    let Some(hwnd) = slint_hwnd(ui) else { return };
    unsafe {
        let _ = ReleaseCapture();
        SendMessageW(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
        // The Win32 modal drag loop swallows the mouse-up that ends the drag,
        // so Slint's TouchAreas remain stuck in "left-button pressed" state.
        // Symptom: every subsequent click on refresh / min / close / checkbox /
        // account-switch circle is silently ignored. Posting a synthetic
        // WM_LBUTTONUP forces Slint to finish the press cycle and reset.
        let _ = PostMessageW(hwnd, WM_LBUTTONUP, WPARAM(0), LPARAM(0));
    }
}

/// Native input dialog. Uses PowerShell's `Read-Host` is brittle in GUI mode;
/// instead we prompt via VBScript + InputBox which works without a console.
async fn show_input_dialog(title: &str, prompt: &str, default: &str) -> Option<String> {
    #[cfg(windows)]
    {
        let title = title.to_string();
        let prompt = prompt.to_string();
        let default = default.to_string();
        let result = tokio::task::spawn_blocking(move || -> Option<String> {
            // Generate a temp .vbs that writes user input to a temp file.
            let dir = std::env::temp_dir();
            let vbs = dir.join(format!("cmon_input_{}.vbs", std::process::id()));
            let out = dir.join(format!("cmon_input_{}.txt", std::process::id()));
            let escape = |s: &str| s.replace('"', "\"\"");
            let body = format!(
                "Dim r\r\n\
                 r = InputBox(\"{}\", \"{}\", \"{}\")\r\n\
                 If r <> \"\" Or InStr(r, \"\") = 1 Then\r\n\
                   Dim fso, f\r\n\
                   Set fso = CreateObject(\"Scripting.FileSystemObject\")\r\n\
                   Set f = fso.CreateTextFile(\"{}\", True, True)\r\n\
                   f.Write r\r\n\
                   f.Close\r\n\
                 End If\r\n",
                escape(&prompt),
                escape(&title),
                escape(&default),
                out.to_string_lossy().replace('\\', "\\\\"),
            );
            std::fs::write(&vbs, body).ok()?;
            let _ = std::process::Command::new("wscript.exe")
                .arg(&vbs)
                .status()
                .ok()?;
            let result = std::fs::read_to_string(&out).ok();
            // Clean up.
            let _ = std::fs::remove_file(&vbs);
            let _ = std::fs::remove_file(&out);
            result.map(|s| s.trim().to_string())
        })
        .await
        .ok()
        .flatten();
        return result;
    }
    #[cfg(not(windows))]
    {
        let _ = (title, prompt, default);
        None
    }
}
