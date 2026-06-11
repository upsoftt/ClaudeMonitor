use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration as StdDuration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;

use crate::types::MetricBucket;

const LOG_TAIL_BYTES: u64 = 32 * 1024 * 1024;
const RATE_LIMIT_MARKER: &str = r#"{"type":"codex.rate_limits""#;
const WEBSOCKET_EVENT_MARKER: &str = "websocket event:";
const THREAD_ID_LOOKBACK_BYTES: usize = 16 * 1024;
const HEADER_PRIMARY_USED: &str = "x-codex-primary-used-percent";
const HEADER_SECONDARY_USED: &str = "x-codex-secondary-used-percent";
const HEADER_PRIMARY_RESET_AT: &str = "x-codex-primary-reset-at";
const HEADER_SECONDARY_RESET_AT: &str = "x-codex-secondary-reset-at";
const HEADER_PRIMARY_RESET_AFTER: &str = "x-codex-primary-reset-after-seconds";
const HEADER_SECONDARY_RESET_AFTER: &str = "x-codex-secondary-reset-after-seconds";
const HEADER_PLAN_TYPE: &str = "x-codex-plan-type";
const APP_SERVER_TIMEOUT: StdDuration = StdDuration::from_secs(18);
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Default, Clone)]
pub struct CodexUsageSnapshot {
    pub email: Option<String>,
    pub plan: String,
    pub session: Option<String>,
    pub five_hour: Option<MetricBucket>,
    pub seven_day: Option<MetricBucket>,
}

impl CodexUsageSnapshot {
    pub fn from_rate_limits_event(raw: &str, session: Option<&str>) -> Result<Self> {
        let event: CodexRateLimitsEvent = serde_json::from_str(raw)
            .with_context(|| format!("parse codex rate limits event: {raw}"))?;
        let five_hour = event.rate_limits.primary.as_ref().map(window_to_metric);
        let seven_day = event.rate_limits.secondary.as_ref().map(window_to_metric);
        Ok(Self {
            email: None,
            plan: plan_label(&event.plan_type),
            session: session.map(str::to_string),
            five_hour,
            seven_day,
        })
    }
}

#[derive(Debug, Deserialize)]
struct CodexRateLimitsEvent {
    #[serde(default)]
    plan_type: String,
    #[serde(default)]
    rate_limits: CodexRateLimits,
}

#[derive(Debug, Default, Deserialize)]
struct CodexRateLimits {
    #[serde(default)]
    primary: Option<CodexRateWindow>,
    #[serde(default)]
    secondary: Option<CodexRateWindow>,
}

#[derive(Debug, Deserialize)]
struct CodexRateWindow {
    #[serde(default)]
    used_percent: f64,
    #[serde(default)]
    reset_at: Option<i64>,
    #[serde(default)]
    reset_after_seconds: Option<i64>,
}

#[derive(Debug, Default)]
struct CodexAuthInfo {
    email: Option<String>,
    plan: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    #[serde(default)]
    tokens: Option<CodexAuthTokens>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthTokens {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SqliteLogRow {
    #[serde(default)]
    thread_id: String,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize)]
struct AppServerEnvelope {
    id: Option<u64>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct AppServerAccountRead {
    account: Option<AppServerAccount>,
}

#[derive(Debug, Deserialize)]
struct AppServerAccount {
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "planType")]
    plan_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AppServerRateLimitsRead {
    #[serde(rename = "rateLimits")]
    rate_limits: AppServerRateLimitSnapshot,
}

#[derive(Debug, Deserialize)]
struct AppServerRateLimitSnapshot {
    #[serde(default, rename = "planType")]
    plan_type: Option<String>,
    #[serde(default)]
    primary: Option<AppServerRateWindow>,
    #[serde(default)]
    secondary: Option<AppServerRateWindow>,
}

#[derive(Debug, Deserialize)]
struct AppServerRateWindow {
    #[serde(rename = "usedPercent")]
    used_percent: f64,
    #[serde(rename = "resetsAt")]
    resets_at: i64,
}

pub fn load_current() -> Result<CodexUsageSnapshot> {
    let codex_dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("home dir not available"))?
        .join(".codex");
    load_from_codex_dir(&codex_dir)
}

pub fn load_from_codex_dir(codex_dir: &Path) -> Result<CodexUsageSnapshot> {
    let auth = std::fs::read_to_string(codex_dir.join("auth.json"))
        .ok()
        .and_then(|raw| parse_auth_info(&raw).ok())
        .unwrap_or_default();

    let now_ts = Utc::now().timestamp();
    let mut usage = latest_snapshot_from_app_server(now_ts)
        .or_else(|_| latest_snapshot_from_sqlite(codex_dir, now_ts))
        .or_else(|_| {
            let mut log_text =
                read_log_tail(&codex_dir.join("logs_2.sqlite"), LOG_TAIL_BYTES).unwrap_or_default();
            if let Ok(wal_text) =
                read_log_tail(&codex_dir.join("logs_2.sqlite-wal"), LOG_TAIL_BYTES)
            {
                log_text.push('\n');
                log_text.push_str(&wal_text);
            }
            latest_snapshot_from_log_text_at(&log_text, now_ts)
        })
        .unwrap_or_default();

    if usage.email.is_none() {
        usage.email = auth.email;
    }
    if usage.plan.is_empty() {
        usage.plan = auth.plan.unwrap_or_else(|| "Codex".to_string());
    }
    Ok(usage)
}

fn read_log_tail(path: &Path, max_bytes: u64) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn latest_snapshot_from_app_server(now_ts: i64) -> Result<CodexUsageSnapshot> {
    let codex = find_codex_binary().ok_or_else(|| anyhow!("codex binary not found"))?;
    let mut command = Command::new(&codex);
    command
        .arg("app-server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        command.creation_flags(codex_app_server_creation_flags());
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn {}", codex.display()))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("codex app-server stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("codex app-server stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("codex app-server stderr unavailable"))?;

    let (line_tx, line_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let (err_tx, err_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut err = String::new();
        let _ = reader.read_to_string(&mut err);
        let _ = err_tx.send(err);
    });

    let init = r#"{"method":"initialize","id":0,"params":{"clientInfo":{"name":"claude_monitor","title":"ClaudeMonitor","version":"0.1.0"},"capabilities":null}}"#;
    writeln!(stdin, "{init}")?;
    stdin.flush()?;
    let mut account: Option<AppServerAccountRead> = None;
    let mut limits: Option<AppServerRateLimitsRead> = None;
    let deadline = std::time::Instant::now() + APP_SERVER_TIMEOUT;

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            cleanup_child(&mut child);
            let stderr = err_rx.try_recv().unwrap_or_default();
            return Err(anyhow!("codex app-server initialize timed out: {stderr}"));
        }
        let line = line_rx.recv_timeout(remaining)?;
        let env: AppServerEnvelope = serde_json::from_str(&line)
            .with_context(|| format!("parse codex app-server line: {line}"))?;
        if env.id == Some(0) {
            if let Some(error) = env.error {
                cleanup_child(&mut child);
                return Err(anyhow!("codex app-server initialize error: {error}"));
            }
            break;
        }
    }

    writeln!(stdin, "{}", r#"{"method":"initialized"}"#)?;
    writeln!(
        stdin,
        "{}",
        r#"{"method":"account/read","id":1,"params":{"refreshToken":false}}"#
    )?;
    writeln!(
        stdin,
        "{}",
        r#"{"method":"account/rateLimits/read","id":2}"#
    )?;
    stdin.flush()?;

    while account.is_none() || limits.is_none() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            cleanup_child(&mut child);
            let stderr = err_rx.try_recv().unwrap_or_default();
            return Err(anyhow!("codex app-server rate limits timed out: {stderr}"));
        }
        let line = line_rx.recv_timeout(remaining)?;
        let env: AppServerEnvelope = serde_json::from_str(&line)
            .with_context(|| format!("parse codex app-server line: {line}"))?;
        if let Some(error) = env.error {
            cleanup_child(&mut child);
            return Err(anyhow!("codex app-server request error: {error}"));
        }
        match env.id {
            Some(1) => {
                let result = env
                    .result
                    .ok_or_else(|| anyhow!("account/read missing result"))?;
                account = Some(serde_json::from_value(result)?);
            }
            Some(2) => {
                let result = env
                    .result
                    .ok_or_else(|| anyhow!("account/rateLimits/read missing result"))?;
                limits = Some(serde_json::from_value(result)?);
            }
            _ => {}
        }
    }

    cleanup_child(&mut child);
    snapshot_from_app_server(account.as_ref(), limits.as_ref().unwrap(), now_ts)
}

fn cleanup_child(child: &mut std::process::Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
fn codex_app_server_creation_flags() -> u32 {
    CREATE_NO_WINDOW
}

fn find_codex_binary() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        let npm = PathBuf::from(appdata).join("npm");
        candidates.push(
            npm.join("node_modules")
                .join("@openai")
                .join("codex")
                .join("node_modules")
                .join("@openai")
                .join("codex-win32-x64")
                .join("vendor")
                .join("x86_64-pc-windows-msvc")
                .join("bin")
                .join("codex.exe"),
        );
        candidates.push(
            npm.join("node_modules")
                .join("@openai")
                .join("codex")
                .join("vendor")
                .join("x86_64-pc-windows-msvc")
                .join("bin")
                .join("codex.exe"),
        );
        candidates.push(npm.join("codex.cmd"));
    }
    candidates.push(PathBuf::from("codex"));
    candidates.into_iter().find(|p| {
        p.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "codex")
            || p.exists()
    })
}

fn latest_snapshot_from_sqlite(codex_dir: &Path, now_ts: i64) -> Result<CodexUsageSnapshot> {
    let db_path = codex_dir.join("logs_2.sqlite");
    let sql = r#"select json_object('thread_id', ifnull(thread_id,''), 'body', ifnull(feedback_log_body,'')) from logs where (target='codex_client::default_client' and feedback_log_body like '%"x-codex-primary-used-percent":%') or (target='codex_api::endpoint::responses_websocket' and feedback_log_body like '%websocket event: {"type":"codex.rate_limits"%') order by ts desc, ts_nanos desc, id desc limit 80;"#;
    let output = Command::new("sqlite3")
        .arg(&db_path)
        .arg(sql)
        .output()
        .with_context(|| format!("run sqlite3 for {}", db_path.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "sqlite3 failed for {}: {}",
            db_path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut rows = Vec::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        if let Ok(row) = serde_json::from_str::<SqliteLogRow>(line) {
            rows.push(row);
        }
    }
    latest_snapshot_from_log_rows(&rows, now_ts)
}

fn latest_snapshot_from_log_rows(rows: &[SqliteLogRow], now_ts: i64) -> Result<CodexUsageSnapshot> {
    for row in rows {
        if let Ok(snapshot) = snapshot_from_response_headers(
            &row.body,
            (!row.thread_id.is_empty()).then_some(row.thread_id.as_str()),
            now_ts,
        ) {
            return Ok(snapshot);
        }
        if let Ok(snapshot) = latest_snapshot_from_log_text_at(&row.body, now_ts).or_else(|_| {
            let Some(start) = row.body.find(RATE_LIMIT_MARKER) else {
                return Err(anyhow!("codex.rate_limits event not found"));
            };
            let event = extract_json_object_at(&row.body[start..])?;
            CodexUsageSnapshot::from_rate_limits_event(
                event,
                (!row.thread_id.is_empty()).then_some(row.thread_id.as_str()),
            )
        }) {
            if snapshot_rank(&snapshot, 0, now_ts).active_window {
                return Ok(snapshot);
            }
        }
    }
    Err(anyhow!("fresh codex usage event not found"))
}

#[cfg(test)]
fn latest_snapshot_from_log_texts<'a>(texts: impl IntoIterator<Item = &'a str>) -> String {
    let mut combined = String::new();
    for text in texts {
        combined.push_str(text);
        combined.push('\n');
    }
    combined
}

fn latest_snapshot_from_log_text_at(text: &str, now_ts: i64) -> Result<CodexUsageSnapshot> {
    let mut search_from = 0usize;
    let mut best: Option<(SnapshotRank, CodexUsageSnapshot)> = None;
    while let Some(relative_start) = text[search_from..].find(RATE_LIMIT_MARKER) {
        let start = search_from + relative_start;
        search_from = start + RATE_LIMIT_MARKER.len();
        if !is_websocket_event_marker(text, start) {
            continue;
        }
        let Ok(event) = extract_json_object_at(&text[start..]) else {
            continue;
        };
        let session = latest_thread_id_before(&text[..start]);
        let Ok(snapshot) = CodexUsageSnapshot::from_rate_limits_event(event, session.as_deref())
        else {
            continue;
        };
        let rank = snapshot_rank(&snapshot, start, now_ts);
        if !rank.active_window {
            continue;
        }
        if best
            .as_ref()
            .map_or(true, |(best_rank, _)| rank > *best_rank)
        {
            best = Some((rank, snapshot));
        }
    }
    best.map(|(_, snapshot)| snapshot)
        .ok_or_else(|| anyhow!("codex.rate_limits event not found"))
}

fn is_websocket_event_marker(text: &str, marker_start: usize) -> bool {
    let context_start = floor_char_boundary(text, marker_start.saturating_sub(160));
    text[context_start..marker_start].contains(WEBSOCKET_EVENT_MARKER)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SnapshotRank {
    active_window: bool,
    primary_reset: i64,
    secondary_reset: i64,
    marker_start: usize,
}

fn snapshot_rank(snapshot: &CodexUsageSnapshot, marker_start: usize, now_ts: i64) -> SnapshotRank {
    let primary_reset = reset_ts(snapshot.five_hour.as_ref()).unwrap_or(0);
    let secondary_reset = reset_ts(snapshot.seven_day.as_ref()).unwrap_or(0);
    SnapshotRank {
        active_window: primary_reset >= now_ts,
        primary_reset,
        secondary_reset,
        marker_start,
    }
}

fn reset_ts(metric: Option<&MetricBucket>) -> Option<i64> {
    metric
        .and_then(|m| m.resets_at.as_deref())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .map(|dt| dt.timestamp())
}

fn snapshot_from_app_server(
    account: Option<&AppServerAccountRead>,
    rate_limits: &AppServerRateLimitsRead,
    now_ts: i64,
) -> Result<CodexUsageSnapshot> {
    let limits = &rate_limits.rate_limits;
    let primary = limits
        .primary
        .as_ref()
        .ok_or_else(|| anyhow!("codex app-server primary limit missing"))?;
    let secondary = limits.secondary.as_ref();
    let plan = limits
        .plan_type
        .as_deref()
        .or_else(|| {
            account
                .and_then(|a| a.account.as_ref())
                .and_then(|a| a.plan_type.as_deref())
        })
        .map(plan_label)
        .unwrap_or_else(|| "Codex".to_string());
    let email = account
        .and_then(|a| a.account.as_ref())
        .and_then(|a| a.email.clone());
    let snapshot = CodexUsageSnapshot {
        email,
        plan,
        session: None,
        five_hour: Some(metric_from_app_server_window(primary)),
        seven_day: secondary.map(metric_from_app_server_window),
    };
    if !snapshot_rank(&snapshot, 0, now_ts).active_window {
        return Err(anyhow!("codex app-server primary window is expired"));
    }
    Ok(snapshot)
}

fn metric_from_app_server_window(window: &AppServerRateWindow) -> MetricBucket {
    MetricBucket {
        utilization: window.used_percent.clamp(0.0, 100.0),
        resets_at: DateTime::<Utc>::from_timestamp(window.resets_at, 0).map(|dt| dt.to_rfc3339()),
        used: None,
        used_limit: None,
    }
}

fn snapshot_from_response_headers(
    text: &str,
    session: Option<&str>,
    now_ts: i64,
) -> Result<CodexUsageSnapshot> {
    let primary_used = header_f64(text, HEADER_PRIMARY_USED)?;
    let secondary_used = header_f64(text, HEADER_SECONDARY_USED)?;
    let primary_reset = header_reset_ts(
        text,
        HEADER_PRIMARY_RESET_AT,
        HEADER_PRIMARY_RESET_AFTER,
        now_ts,
    )?;
    let secondary_reset = header_reset_ts(
        text,
        HEADER_SECONDARY_RESET_AT,
        HEADER_SECONDARY_RESET_AFTER,
        now_ts,
    )?;
    let plan = header_value(text, HEADER_PLAN_TYPE)
        .map(plan_label)
        .unwrap_or_else(|| "Codex".to_string());
    let snapshot = CodexUsageSnapshot {
        email: None,
        plan,
        session: session.map(str::to_string),
        five_hour: Some(metric_from_header(primary_used, primary_reset)),
        seven_day: Some(metric_from_header(secondary_used, secondary_reset)),
    };
    if !snapshot_rank(&snapshot, 0, now_ts).active_window {
        return Err(anyhow!("codex response header window is expired"));
    }
    Ok(snapshot)
}

fn header_f64(text: &str, key: &str) -> Result<f64> {
    header_value(text, key)
        .ok_or_else(|| anyhow!("missing codex response header {key}"))?
        .parse::<f64>()
        .with_context(|| format!("parse codex response header {key}"))
}

fn header_reset_ts(
    text: &str,
    reset_at_key: &str,
    reset_after_key: &str,
    now_ts: i64,
) -> Result<i64> {
    if let Some(reset_at) = header_value(text, reset_at_key) {
        return reset_at
            .parse::<i64>()
            .with_context(|| format!("parse codex response header {reset_at_key}"));
    }
    let reset_after = header_value(text, reset_after_key)
        .ok_or_else(|| anyhow!("missing codex response header {reset_at_key}"))?
        .parse::<i64>()
        .with_context(|| format!("parse codex response header {reset_after_key}"))?;
    Ok(now_ts + reset_after)
}

fn header_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!(r#""{key}": ""#);
    let start = text.find(&marker)? + marker.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn metric_from_header(used_percent: f64, reset_ts: i64) -> MetricBucket {
    MetricBucket {
        utilization: used_percent.clamp(0.0, 100.0),
        resets_at: DateTime::<Utc>::from_timestamp(reset_ts, 0).map(|dt| dt.to_rfc3339()),
        used: None,
        used_limit: None,
    }
}

fn extract_json_object_at(text: &str) -> Result<&str> {
    if !text.starts_with('{') {
        return Err(anyhow!("JSON object must start with '{{'"));
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (idx, ch) in text.char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&text[..=idx]);
                }
            }
            _ => {}
        }
    }
    Err(anyhow!("unterminated JSON object"))
}

fn latest_thread_id_before(text: &str) -> Option<String> {
    let start = floor_char_boundary(text, text.len().saturating_sub(THREAD_ID_LOOKBACK_BYTES));
    let tail = &text[start..];
    let idx = tail.rfind("thread_id=")? + "thread_id=".len();
    let id: String = tail[idx..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (!id.is_empty()).then_some(id)
}

fn floor_char_boundary(text: &str, idx: usize) -> usize {
    let mut idx = idx.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn window_to_metric(window: &CodexRateWindow) -> MetricBucket {
    MetricBucket {
        utilization: window.used_percent.clamp(0.0, 100.0),
        resets_at: window_reset_iso(window),
        used: None,
        used_limit: None,
    }
}

fn window_reset_iso(window: &CodexRateWindow) -> Option<String> {
    if let Some(ts) = window.reset_at {
        return DateTime::<Utc>::from_timestamp(ts, 0).map(|dt| dt.to_rfc3339());
    }
    let secs = window.reset_after_seconds?;
    Some((Utc::now() + ChronoDuration::seconds(secs)).to_rfc3339())
}

fn parse_auth_info(raw: &str) -> Result<CodexAuthInfo> {
    let auth: CodexAuthFile = serde_json::from_str(raw)?;
    let Some(tokens) = auth.tokens else {
        return Ok(CodexAuthInfo::default());
    };
    let token = tokens.id_token.or(tokens.access_token).unwrap_or_default();
    if token.is_empty() {
        return Ok(CodexAuthInfo::default());
    }
    let payload = decode_jwt_payload(&token)?;
    let email = payload
        .get("email")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            payload
                .get("https://api.openai.com/profile")
                .and_then(|v| v.get("email"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let plan = payload
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_plan_type"))
        .and_then(|v| v.as_str())
        .map(plan_label);
    Ok(CodexAuthInfo { email, plan })
}

fn decode_jwt_payload(token: &str) -> Result<serde_json::Value> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("JWT payload segment missing"))?;
    let bytes = base64_url_decode(payload)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn plan_label(raw: &str) -> String {
    match raw.to_ascii_lowercase().replace(['_', '-'], "").as_str() {
        "prolite" => "Pro Lite".into(),
        "plus" => "Plus".into(),
        "pro" => "Pro".into(),
        "team" => "Team".into(),
        "business" => "Business".into(),
        "enterprise" => "Enterprise".into(),
        "" => "Codex".into(),
        _ => raw.to_string(),
    }
}

fn base64_url_decode(input: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for b in input.bytes() {
        if b == b'=' {
            break;
        }
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err(anyhow!("invalid base64url byte: {b}")),
        } as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
fn base64_url_no_pad(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT_OLD: &str = r#"{"type":"codex.rate_limits","plan_type":"prolite","rate_limits":{"allowed":true,"limit_reached":false,"primary":{"used_percent":61,"window_minutes":300,"reset_after_seconds":2379,"reset_at":1780885747},"secondary":{"used_percent":10,"window_minutes":10080,"reset_after_seconds":589179,"reset_at":1781472547}}}"#;
    const EVENT_NEW: &str = r#"{"type":"codex.rate_limits","plan_type":"prolite","rate_limits":{"allowed":true,"limit_reached":false,"primary":{"used_percent":72,"window_minutes":300,"reset_after_seconds":1800,"reset_at":1780889000},"secondary":{"used_percent":15,"window_minutes":10080,"reset_after_seconds":580000,"reset_at":1781479000}}}"#;
    const EVENT_CURRENT_WINDOW: &str = r#"{"type":"codex.rate_limits","plan_type":"prolite","rate_limits":{"allowed":true,"limit_reached":false,"primary":{"used_percent":30,"window_minutes":300,"reset_at":2000},"secondary":{"used_percent":21,"window_minutes":10080,"reset_at":9000}}}"#;
    const EVENT_EXPIRED_WINDOW: &str = r#"{"type":"codex.rate_limits","plan_type":"prolite","rate_limits":{"allowed":true,"limit_reached":false,"primary":{"used_percent":61,"window_minutes":300,"reset_at":1000},"secondary":{"used_percent":10,"window_minutes":10080,"reset_at":8000}}}"#;
    const HEADER_LOG: &str = r#"Request completed method=POST status=200 OK headers={"x-codex-active-limit": "premium", "x-codex-plan-type": "prolite", "x-codex-primary-used-percent": "41", "x-codex-secondary-used-percent": "23", "x-codex-primary-window-minutes": "300", "x-codex-secondary-window-minutes": "10080", "x-codex-primary-reset-after-seconds": "16834", "x-codex-secondary-reset-after-seconds": "585508", "x-codex-primary-reset-at": "1780903874", "x-codex-secondary-reset-at": "1781472547"} version=HTTP/1.1"#;
    const APP_SERVER_ACCOUNT_JSON: &str = r#"{"account":{"type":"chatgpt","email":"rumo@example.test","planType":"prolite"},"requiresOpenaiAuth":true}"#;
    const APP_SERVER_LIMITS_JSON: &str = r#"{"rateLimits":{"limitId":"codex","limitName":null,"primary":{"usedPercent":65,"windowDurationMins":300,"resetsAt":1780903874},"secondary":{"usedPercent":27,"windowDurationMins":10080,"resetsAt":1781472547},"credits":{"hasCredits":false,"unlimited":false,"balance":"0"},"individualLimit":null,"planType":"prolite","rateLimitReachedType":null},"rateLimitsByLimitId":{}}"#;

    #[test]
    fn parses_codex_rate_limits_as_used_percent_metrics() {
        let usage = CodexUsageSnapshot::from_rate_limits_event(
            EVENT_OLD,
            Some("019ea4c8-8777-7980-9c56-0ca444d5ebd0"),
        )
        .expect("rate limit event should parse");

        assert_eq!(usage.plan, "Pro Lite");
        assert_eq!(
            usage.session.as_deref(),
            Some("019ea4c8-8777-7980-9c56-0ca444d5ebd0")
        );
        assert!((usage.five_hour.as_ref().unwrap().percent() - 61.0).abs() < 1e-6);
        assert!((usage.seven_day.as_ref().unwrap().percent() - 10.0).abs() < 1e-6);
        assert_eq!(
            usage.five_hour.as_ref().unwrap().resets_at.as_deref(),
            Some("2026-06-08T02:29:07+00:00")
        );
    }

    #[test]
    fn extracts_latest_codex_rate_limit_event_from_log_text() {
        let log = format!(
            "thread_id=old: websocket event: {EVENT_OLD}\nnoise\nthread_id=019ea4c8-8777-7980-9c56-0ca444d5ebd0: websocket event: {EVENT_NEW}"
        );

        let usage =
            latest_snapshot_from_log_text_at(&log, 1780885000).expect("latest event should parse");

        assert_eq!(
            usage.session.as_deref(),
            Some("019ea4c8-8777-7980-9c56-0ca444d5ebd0")
        );
        assert!((usage.five_hour.as_ref().unwrap().percent() - 72.0).abs() < 1e-6);
        assert!((usage.seven_day.as_ref().unwrap().percent() - 15.0).abs() < 1e-6);
    }

    #[test]
    fn extracts_session_when_thread_id_is_far_before_rate_limit_event() {
        let padding = "x".repeat(5_000);
        let log = format!(
            "thread_id=019ea4c8-8777-7980-9c56-0ca444d5ebd0:{padding} websocket event: {EVENT_NEW}"
        );

        let usage =
            latest_snapshot_from_log_text_at(&log, 1780885000).expect("latest event should parse");

        assert_eq!(
            usage.session.as_deref(),
            Some("019ea4c8-8777-7980-9c56-0ca444d5ebd0")
        );
    }

    #[test]
    fn prefers_newer_rate_limit_event_from_later_log_text() {
        let main_log = format!("thread_id=old-session: websocket event: {EVENT_OLD}");
        let wal_log =
            format!("thread_id=019ea4c8-8777-7980-9c56-0ca444d5ebd0: websocket event: {EVENT_NEW}");

        let combined = latest_snapshot_from_log_texts([main_log.as_str(), wal_log.as_str()]);
        let usage = latest_snapshot_from_log_text_at(&combined, 1780885000)
            .expect("latest event should parse across log texts");

        assert_eq!(
            usage.session.as_deref(),
            Some("019ea4c8-8777-7980-9c56-0ca444d5ebd0")
        );
        assert!((usage.five_hour.as_ref().unwrap().percent() - 72.0).abs() < 1e-6);
        assert!((usage.seven_day.as_ref().unwrap().percent() - 15.0).abs() < 1e-6);
    }

    #[test]
    fn ignores_expired_rate_limit_window_even_if_it_appears_later_in_raw_log() {
        let log = format!(
            "thread_id=019ea4ff-c20b-7450-92c6-4771e6e4ad28: websocket event: {EVENT_CURRENT_WINDOW}\n\
             thread_id=019ea4c8-8777-7980-9c56-0ca444d5ebd0: websocket event: {EVENT_EXPIRED_WINDOW}"
        );

        let usage =
            latest_snapshot_from_log_text_at(&log, 1500).expect("active event should parse");

        assert_eq!(
            usage.session.as_deref(),
            Some("019ea4ff-c20b-7450-92c6-4771e6e4ad28")
        );
        assert!((usage.five_hour.as_ref().unwrap().percent() - 30.0).abs() < 1e-6);
        assert!((usage.seven_day.as_ref().unwrap().percent() - 21.0).abs() < 1e-6);
    }

    #[test]
    fn ignores_rate_limit_json_that_is_not_a_websocket_event() {
        let log = format!(
            "const EVENT: &str = r#\"{EVENT_EXPIRED_WINDOW}\"#;\n\
             thread_id=019ea4ff-c20b-7450-92c6-4771e6e4ad28: websocket event: {EVENT_CURRENT_WINDOW}"
        );

        let usage = latest_snapshot_from_log_text_at(&log, 1500)
            .expect("real websocket event should parse");

        assert!((usage.five_hour.as_ref().unwrap().percent() - 30.0).abs() < 1e-6);
        assert!((usage.seven_day.as_ref().unwrap().percent() - 21.0).abs() < 1e-6);
    }

    #[test]
    fn parses_codex_response_headers_as_used_percent_metrics() {
        let usage = snapshot_from_response_headers(
            HEADER_LOG,
            Some("019ea48e-378b-7ca1-920f-4e8bd3afb46f"),
            1780888000,
        )
        .expect("response headers should parse");

        assert_eq!(usage.plan, "Pro Lite");
        assert_eq!(
            usage.session.as_deref(),
            Some("019ea48e-378b-7ca1-920f-4e8bd3afb46f")
        );
        assert!((usage.five_hour.as_ref().unwrap().percent() - 41.0).abs() < 1e-6);
        assert!((usage.seven_day.as_ref().unwrap().percent() - 23.0).abs() < 1e-6);
        assert_eq!(
            usage.five_hour.as_ref().unwrap().resets_at.as_deref(),
            Some("2026-06-08T07:31:14+00:00")
        );
    }

    #[test]
    fn parses_app_server_rate_limits_as_used_percent_metrics() {
        let account: AppServerAccountRead = serde_json::from_str(APP_SERVER_ACCOUNT_JSON).unwrap();
        let limits: AppServerRateLimitsRead = serde_json::from_str(APP_SERVER_LIMITS_JSON).unwrap();

        let usage = snapshot_from_app_server(Some(&account), &limits, 1780888000)
            .expect("app-server response should parse");

        assert_eq!(usage.email.as_deref(), Some("rumo@example.test"));
        assert_eq!(usage.plan, "Pro Lite");
        assert!((usage.five_hour.as_ref().unwrap().percent() - 65.0).abs() < 1e-6);
        assert!((usage.seven_day.as_ref().unwrap().percent() - 27.0).abs() < 1e-6);
        assert_eq!(
            usage.five_hour.as_ref().unwrap().resets_at.as_deref(),
            Some("2026-06-08T07:31:14+00:00")
        );
    }

    #[cfg(windows)]
    #[test]
    fn codex_app_server_is_spawned_without_console_window() {
        assert_ne!(codex_app_server_creation_flags() & CREATE_NO_WINDOW, 0);
    }

    #[test]
    fn sqlite_rows_prefer_fresh_response_headers_over_expired_websocket_event() {
        let rows = vec![
            SqliteLogRow {
                thread_id: "019ea48e-378b-7ca1-920f-4e8bd3afb46f".into(),
                body: HEADER_LOG.into(),
            },
            SqliteLogRow {
                thread_id: "019ea4c8-8777-7980-9c56-0ca444d5ebd0".into(),
                body: format!("websocket event: {EVENT_EXPIRED_WINDOW}"),
            },
        ];

        let usage = latest_snapshot_from_log_rows(&rows, 1780888000)
            .expect("fresh header row should parse");

        assert!((usage.five_hour.as_ref().unwrap().percent() - 41.0).abs() < 1e-6);
        assert!((usage.seven_day.as_ref().unwrap().percent() - 23.0).abs() < 1e-6);
    }

    #[test]
    fn does_not_return_only_expired_rate_limit_window() {
        let log = format!(
            "thread_id=019ea4c8-8777-7980-9c56-0ca444d5ebd0: websocket event: {EVENT_EXPIRED_WINDOW}"
        );

        let err = latest_snapshot_from_log_text_at(&log, 1500)
            .expect_err("expired event should not be used")
            .to_string();

        assert!(err.contains("codex.rate_limits event not found"));
    }

    #[test]
    fn handles_non_ascii_text_around_log_marker() {
        let log = format!(
            "a{} websocket event: {EVENT_CURRENT_WINDOW}",
            "й".repeat(80)
        );

        let usage =
            latest_snapshot_from_log_text_at(&log, 1500).expect("active event should parse");

        assert!((usage.five_hour.as_ref().unwrap().percent() - 30.0).abs() < 1e-6);
    }

    #[test]
    fn floors_byte_offsets_to_utf8_char_boundaries() {
        assert_eq!(floor_char_boundary("aй", 2), 1);
        assert_eq!(floor_char_boundary("aй", 3), 3);
    }

    #[test]
    fn extracts_email_and_plan_from_codex_auth_jwt_payload() {
        let payload = r#"{"email":"rumo@example.test","https://api.openai.com/auth":{"chatgpt_plan_type":"prolite"}}"#;
        let token = format!("header.{}.sig", base64_url_no_pad(payload.as_bytes()));
        let auth = format!(r#"{{"tokens":{{"id_token":"{token}"}}}}"#);

        let info = parse_auth_info(&auth).expect("auth json should parse");

        assert_eq!(info.email.as_deref(), Some("rumo@example.test"));
        assert_eq!(info.plan.as_deref(), Some("Pro Lite"));
    }
}
