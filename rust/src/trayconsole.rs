//! TrayConsole client — connect to a Named Pipe, accept newline-delimited
//! commands, reply with JSON, write a periodic heartbeat file, and hold a
//! Named Mutex marking us alive.
//!
//! Wire protocol mirrors `trayconsole_client.py` exactly so projects can be
//! migrated without changes on the TrayConsole side:
//!   pipe path:  \\.\pipe\<pipe_name>
//!   inbound:    one command per line (UTF-8)
//!   outbound:   one JSON object per line
//!   commands:   status, shutdown, custom:<name>
//!   heartbeat:  %LOCALAPPDATA%\TrayConsole\heartbeats\<pipe_name>.json (5s)
//!   mutex:      Global\TrayConsole_<pipe_name>

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

const HEARTBEAT_INTERVAL_SECS: u64 = 5;

#[derive(Debug, Clone)]
pub enum TrayCommand {
    Show,
    Hide,
    Refresh,
    Relogin,
    Stop,
    /// Unhandled custom:* command — passed through verbatim.
    Custom(String),
}

impl TrayCommand {
    fn from_wire(line: &str) -> Self {
        match line {
            "shutdown" => TrayCommand::Stop,
            "show" | "custom:show" => TrayCommand::Show,
            "hide" | "custom:hide" => TrayCommand::Hide,
            "refresh" | "custom:refresh" => TrayCommand::Refresh,
            "relogin" | "custom:relogin" => TrayCommand::Relogin,
            other => TrayCommand::Custom(other.to_string()),
        }
    }
}

/// Run the TrayConsole client until the cancellation token fires.
/// Forwards every recognised command to `tx`. Reconnects with exponential
/// backoff (2s → 30s) when the pipe drops.
pub async fn run(
    pipe_name: String,
    tx: mpsc::Sender<TrayCommand>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let pipe_path = format!(r"\\.\pipe\{pipe_name}");
    let _mutex = create_mutex(&pipe_name);
    let heartbeat_path = heartbeat_path(&pipe_name)?;
    let hb_handle = spawn_heartbeat(heartbeat_path.clone(), pipe_name.clone(), shutdown.clone());

    let mut delay_secs = 2u64;
    loop {
        if *shutdown.borrow() {
            break;
        }
        match connect_and_serve(&pipe_path, &tx).await {
            Ok(()) => {
                tracing::info!("trayconsole pipe disconnected cleanly");
            }
            Err(e) => {
                if *shutdown.borrow() {
                    break;
                }
                tracing::warn!(error = %e, retry_in = delay_secs, "trayconsole reconnect");
            }
        }
        // Wait with exponential backoff or shutdown.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(delay_secs)) => {}
            _ = shutdown.changed() => break,
        }
        delay_secs = (delay_secs * 2).min(30);
    }

    let _ = std::fs::remove_file(&heartbeat_path);
    hb_handle.abort();
    Ok(())
}

#[cfg(windows)]
async fn connect_and_serve(
    pipe_path: &str,
    tx: &mpsc::Sender<TrayCommand>,
) -> Result<()> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let pipe = ClientOptions::new()
        .open(pipe_path)
        .with_context(|| format!("open {pipe_path}"))?;
    tracing::info!(%pipe_path, "trayconsole connected");

    let (rd, mut wr) = tokio::io::split(pipe);
    let mut reader = BufReader::new(rd).lines();
    while let Some(line) = reader.next_line().await? {
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }
        tracing::debug!(cmd = %raw, "trayconsole command");
        let cmd = TrayCommand::from_wire(raw);
        let resp = match &cmd {
            TrayCommand::Stop => json!({"status": "ok"}),
            TrayCommand::Custom(name) => json!({"status": "ok", "command": name}),
            _ => json!({"status": "ok"}),
        };
        let line_out = format!("{}\n", serde_json::to_string(&resp).unwrap());
        wr.write_all(line_out.as_bytes()).await?;
        wr.flush().await?;

        let _ = tx.send(cmd.clone()).await;
        if matches!(cmd, TrayCommand::Stop) {
            // TrayConsole expects us to exit shortly after acking shutdown.
            tokio::time::sleep(Duration::from_millis(200)).await;
            break;
        }
    }
    Ok(())
}

#[cfg(not(windows))]
async fn connect_and_serve(_pipe_path: &str, _tx: &mpsc::Sender<TrayCommand>) -> Result<()> {
    anyhow::bail!("TrayConsole IPC is Windows-only");
}

// ─────────────────────────────── heartbeat ───────────────────────────────────

fn heartbeat_path(pipe_name: &str) -> Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
        .context("LOCALAPPDATA not set")?;
    let dir = local.join("TrayConsole").join("heartbeats");
    std::fs::create_dir_all(&dir).ok();
    Ok(dir.join(format!("{pipe_name}.json")))
}

fn spawn_heartbeat(
    path: PathBuf,
    pipe_name: String,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let pid = std::process::id();
        loop {
            if *shutdown.borrow() {
                break;
            }
            let body = json!({
                "pid": pid,
                "timestamp": chrono::Utc::now().timestamp() as f64,
                "status": "running",
                "name": pipe_name,
            });
            let _ = write_heartbeat(&path, &body.to_string());
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)) => {}
                _ = shutdown.changed() => break,
            }
        }
    })
}

fn write_heartbeat(path: &std::path::Path, body: &str) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ─────────────────────────────── mutex ───────────────────────────────────────

#[cfg(windows)]
struct MutexHandle(windows::Win32::Foundation::HANDLE);

// SAFETY: Windows HANDLE is just an opaque pointer; sending the owning
// wrapper across threads is safe as long as we never share `&mut HANDLE`
// concurrently. We only call `CloseHandle` once (in Drop), so this holds.
#[cfg(windows)]
unsafe impl Send for MutexHandle {}
#[cfg(windows)]
unsafe impl Sync for MutexHandle {}

#[cfg(windows)]
impl Drop for MutexHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn create_mutex(pipe_name: &str) -> Option<MutexHandle> {
    use windows::core::HSTRING;
    use windows::Win32::System::Threading::CreateMutexW;
    let name = HSTRING::from(format!(r"Global\TrayConsole_{pipe_name}"));
    unsafe {
        match CreateMutexW(None, false, &name) {
            Ok(h) if !h.is_invalid() => {
                tracing::info!(name = %name, "trayconsole mutex created");
                Some(MutexHandle(h))
            }
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = ?e, "CreateMutexW failed");
                None
            }
        }
    }
}

#[cfg(not(windows))]
fn create_mutex(_pipe_name: &str) -> Option<()> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_commands() {
        assert!(matches!(TrayCommand::from_wire("shutdown"), TrayCommand::Stop));
        assert!(matches!(TrayCommand::from_wire("custom:show"), TrayCommand::Show));
        assert!(matches!(TrayCommand::from_wire("custom:refresh"), TrayCommand::Refresh));
        match TrayCommand::from_wire("custom:weird") {
            TrayCommand::Custom(s) => assert_eq!(s, "custom:weird"),
            _ => panic!("expected custom"),
        }
    }
}
