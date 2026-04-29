//! Live smoke test for `api::fetch_usage` + `api::fetch_identity`.
//! Uses the active account from `accounts_meta.json`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use claude_monitor::{api, http_client};

#[tokio::main]
async fn main() -> Result<()> {
    let app_dir: PathBuf = std::env::current_dir()?.parent().unwrap().to_path_buf();
    let meta_raw = std::fs::read_to_string(app_dir.join("accounts_meta.json"))?;
    let meta: serde_json::Value = serde_json::from_str(&meta_raw)?;
    let aid = meta["active"].as_str().context("no active account")?;
    let cookie_file = app_dir.join("accounts").join(format!("{aid}.json"));
    eprintln!("[api_probe] account: {aid} ({})", cookie_file.display());

    let ctx = http_client::load_account_session(&cookie_file)?;
    eprintln!(
        "[api_probe] sessionKey: {} bytes, lastActiveOrg: {}",
        ctx.session_key.len(),
        ctx.last_active_org
    );

    eprintln!("\n[api_probe] fetch_identity()...");
    match api::fetch_identity(&app_dir, &ctx).await {
        Ok(id) => eprintln!(
            "  email: {}\n  name: {}\n  plan: {}\n  uuid: {}",
            id.email, id.display_name, id.plan, id.uuid
        ),
        Err(e) => eprintln!("  ERROR: {e:#}"),
    }

    eprintln!("\n[api_probe] fetch_usage()...");
    match api::fetch_usage(&app_dir, &ctx).await {
        Ok(u) => {
            for (label, m) in [
                ("five_hour       ", &u.five_hour),
                ("seven_day       ", &u.seven_day),
                ("seven_day_omelet", &u.seven_day_omelette),
                ("seven_day_opus  ", &u.seven_day_opus),
                ("seven_day_sonnet", &u.seven_day_sonnet),
                ("seven_day_cowork", &u.seven_day_cowork),
                ("seven_day_oauth ", &u.seven_day_oauth_apps),
            ] {
                match m {
                    Some(b) => eprintln!(
                        "  {label}: {:5.1}% (resets at {})",
                        b.percent(),
                        b.resets_at.as_deref().unwrap_or("—")
                    ),
                    None => eprintln!("  {label}: <null>"),
                }
            }
        }
        Err(e) => eprintln!("  ERROR: {e:#}"),
    }

    eprintln!("\n[api_probe] fetch_incidents()...");
    match api::fetch_incidents().await {
        Ok(list) => {
            if list.is_empty() {
                eprintln!("  (no active incidents)");
            } else {
                for i in &list {
                    eprintln!("  [{}/{}] {}", i.impact, i.status, i.name);
                }
            }
        }
        Err(e) => eprintln!("  ERROR: {e:#}"),
    }
    Ok(())
}
