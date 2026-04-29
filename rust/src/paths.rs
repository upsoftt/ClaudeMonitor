use std::path::PathBuf;

/// Directory containing the executable (or the project root when run via `cargo run`).
pub fn app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `accounts/` directory next to the exe.
pub fn accounts_dir(app_dir: &std::path::Path) -> PathBuf {
    app_dir.join("accounts")
}

/// `accounts_meta.json` file path.
pub fn accounts_meta(app_dir: &std::path::Path) -> PathBuf {
    app_dir.join("accounts_meta.json")
}

/// Legacy `claude_auth.json` — single-account format used before mult-account support.
pub fn legacy_auth(app_dir: &std::path::Path) -> PathBuf {
    app_dir.join("claude_auth.json")
}

/// `proxy.json` — `{"url": "http://..."}`.
pub fn proxy_file(app_dir: &std::path::Path) -> PathBuf {
    app_dir.join("proxy.json")
}

/// `window_state.json` — overlay window position.
pub fn window_state(app_dir: &std::path::Path) -> PathBuf {
    app_dir.join("window_state.json")
}

/// `~/.claude/.credentials.json` — Claude Code OAuth tokens.
pub fn claude_code_creds() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join(".credentials.json"))
}
