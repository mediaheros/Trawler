//! Tauri commands for the first-run setup wizard.

use tauri::State;

use crate::error::{AppError, Result};
use crate::setup::{self, SetupStatus};
use crate::AppState;

#[tauri::command]
pub async fn setup_status(state: State<'_, AppState>) -> Result<SetupStatus> {
    Ok(setup::status(state.inner()).await)
}

#[tauri::command]
pub async fn setup_install_prowlarr(app: tauri::AppHandle) -> Result<String> {
    setup::install_prowlarr(&app).await
}

#[tauri::command]
pub async fn setup_start_prowlarr(state: State<'_, AppState>) -> Result<()> {
    setup::start_managed_prowlarr()?;
    // opportunistically reconnect once it's up
    let state = state.inner();
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let ok = state
            .http
            .get("http://127.0.0.1:9696/ping")
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ok {
            return Ok(());
        }
    }
    Err(AppError::Other("Prowlarr didn't come up within 30 seconds".into()))
}

#[tauri::command]
pub async fn setup_install_qbit() -> Result<()> {
    setup::install_qbt_via_winget()
}

#[tauri::command]
pub async fn setup_configure_qbit() -> Result<()> {
    setup::configure_and_launch_qbt()
}

/// Add a starter set of reliable public indexers (verified in Prowlarr's catalog).
#[tauri::command]
pub async fn setup_starter_indexers(state: State<'_, AppState>) -> Result<Vec<String>> {
    const STARTERS: [&str; 6] =
        ["YTS", "The Pirate Bay", "LimeTorrents", "Knaben", "ExtraTorrent.st", "TorrentsCSV"];
    let mut added = vec![];
    for name in STARTERS {
        match crate::commands::add_indexer(state.clone(), name.to_string()).await {
            Ok(n) => added.push(n),
            Err(_) => continue, // already present or currently unreachable — fine
        }
    }
    Ok(added)
}

#[tauri::command]
pub async fn setup_finish(state: State<'_, AppState>) -> Result<()> {
    let mut cfg = state.config.write().await;
    cfg.setup_completed = true;
    crate::config::save(&cfg)?;
    Ok(())
}

/// Manual RSS sweep (also the live-verification hook).
#[tauri::command]
pub async fn rss_sweep_now(app: tauri::AppHandle) -> Result<serde_json::Value> {
    let stats = crate::rss::sweep(&app).await?;
    Ok(serde_json::json!({
        "releasesSeen": stats.releases_seen,
        "episodeGrabs": stats.episode_grabs,
        "briefGrabs": stats.brief_grabs,
        "briefProposals": stats.brief_proposals,
        "skipped": stats.skipped,
    }))
}
