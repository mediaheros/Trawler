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
pub async fn setup_install_qbit(app: tauri::AppHandle) -> Result<()> {
    setup::install_qbt_via_winget(&app).await
}

#[tauri::command]
pub async fn setup_configure_qbit() -> Result<()> {
    setup::configure_and_launch_qbt()
}

/// Add a starter set of reliable public indexers (verified in Prowlarr's catalog).
/// One catalog fetch for all of them; if NOTHING can be added the real error
/// surfaces instead of a silent empty success.
#[tauri::command]
pub async fn setup_starter_indexers(state: State<'_, AppState>) -> Result<Vec<String>> {
    const STARTERS: [&str; 7] = [
        "YTS",
        "The Pirate Bay",
        "LimeTorrents",
        "Knaben",
        "TorrentsCSV",
        "TorrentProject2",
        "MagnetDownload",
    ];

    let cfg = state.config.read().await.clone();
    if cfg.prowlarr_api_key.is_empty() {
        return Err(AppError::Other(
            "Trawler has no Prowlarr API key yet — paste it on the Prowlarr card first".into(),
        ));
    }
    let client = crate::prowlarr::ProwlarrClient {
        http: &state.http,
        base: cfg.prowlarr_url.clone(),
        api_key: cfg.prowlarr_api_key.clone(),
    };
    // the catalog is ~2 MB — fetch it once, not once per indexer
    let defs = client.schema().await?;

    let mut added = vec![];
    let mut last_err: Option<AppError> = None;
    for name in STARTERS {
        let Some(def) = defs
            .iter()
            .find(|d| d.get("name").and_then(|v| v.as_str()) == Some(name))
        else {
            continue; // renamed/removed from Prowlarr's catalog — skip quietly
        };
        let mut def = def.clone();
        def["enable"] = serde_json::Value::Bool(true);
        def["appProfileId"] = serde_json::Value::from(1);
        match client.add_indexer_raw(&def).await {
            Ok(ok) => added.push(
                ok.get("name").and_then(|v| v.as_str()).unwrap_or(name).to_string(),
            ),
            // an individual failure is fine (already present, site down) —
            // but remember it in case they ALL fail
            Err(e) => last_err = Some(e),
        }
    }
    if added.is_empty() {
        if let Some(e) = last_err {
            return Err(AppError::Other(format!("no indexers could be added: {e}")));
        }
    }
    Ok(added)
}

/// Save the Prowlarr API key from the wizard (for people who already run
/// their own Prowlarr rather than a Trawler-managed one).
#[tauri::command]
pub async fn setup_save_prowlarr_key(state: State<'_, AppState>, key: String) -> Result<()> {
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err(AppError::Other("that key is empty".into()));
    }
    let mut cfg = state.config.write().await;
    cfg.prowlarr_api_key = key;
    crate::config::save(&cfg)?;
    Ok(())
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
