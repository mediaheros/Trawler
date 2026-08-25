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
    setup::start_managed_prowlarr(state.inner()).await
}

#[tauri::command]
pub async fn setup_install_qbit(app: tauri::AppHandle) -> Result<()> {
    setup::install_qbt(&app).await
}

#[tauri::command]
pub async fn setup_configure_qbit(state: State<'_, AppState>) -> Result<()> {
    let qbit_url = state.config.read().await.qbit_url.clone();
    setup::configure_and_launch_qbt(&qbit_url).await?;
    // don't declare victory until the WebUI actually answers
    let cfg = state.config.read().await.clone();
    let q = crate::qbit::QbitClient {
        http: &state.http,
        base: cfg.qbit_url.clone(),
        username: cfg.qbit_username.clone(),
        password: cfg.qbit_password.clone(),
    };
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        if q.version().await.is_ok() {
            // dodge Windows' reserved port ranges while we're here: qBt's own
            // random choice can land inside one (silent WSAEACCES, 0 seeds
            // forever — seen live). Setting it via the API lets qBittorrent
            // persist it into the right place on every platform.
            let cur = q
                .preferences()
                .await
                .ok()
                .and_then(|p| p.get("listen_port").and_then(|v| v.as_u64()))
                .unwrap_or(0) as u16;
            let reserved = tokio::task::spawn_blocking(crate::setup::excluded_port_ranges)
                .await
                .unwrap_or_default();
            if cur == 0 || reserved.iter().any(|(a, b)| cur >= *a && cur <= *b) {
                let port = tokio::task::spawn_blocking(crate::setup::pick_safe_listen_port)
                    .await
                    .unwrap_or(28645);
                if q.set_preferences(&serde_json::json!({ "listen_port": port })).await.is_ok() {
                    crate::applog::info("setup", format!("qbit: listen port {cur} was unusable — set to {port}"));
                }
            }
            return Ok(());
        }
    }
    Err(AppError::Other(
        "qBittorrent launched but its Web UI didn't come up — if it's showing a first-run Legal Notice window, accept it and hit re-check".into(),
    ))
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
    let mut found_in_catalog = 0;
    for name in STARTERS {
        let Some(def) = defs
            .iter()
            .find(|d| d.get("name").and_then(|v| v.as_str()) == Some(name))
        else {
            continue; // renamed/removed from Prowlarr's catalog — skip quietly
        };
        found_in_catalog += 1;
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
        if found_in_catalog == 0 {
            return Err(AppError::Other(
                "Prowlarr's catalog doesn't list Trawler's starter indexers — add one yourself under Settings → Indexers".into(),
            ));
        }
        if let Some(e) = last_err {
            return Err(AppError::Other(format!("no indexers could be added: {e}")));
        }
    }
    Ok(added)
}

/// Cloudflare-protected indexers the unlock flow adds once FlareSolverr is
/// live. Best-effort: Cloudflare's strictness fluctuates per site, so a name
/// failing today is kept for the retry path rather than treated as fatal.
const CF_STARTERS: &[&str] = &["1337x", "EZTV", "ExtraTorrent.st"];

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlaresolverrStatus {
    pub installed: bool,
    pub running: bool,
    pub proxied: bool,
}

#[tauri::command]
pub async fn flaresolverr_status(state: State<'_, AppState>) -> Result<FlaresolverrStatus> {
    let installed = setup::flaresolverr_installed();
    let running = setup::flaresolverr_running(&state).await;
    let proxied = if running {
        let cfg = state.config.read().await.clone();
        match crate::commands::prowlarr_pub(&state.http, &cfg) {
            Ok(client) => client.flaresolverr_proxy_exists().await.unwrap_or(false),
            Err(_) => false,
        }
    } else {
        false
    };
    Ok(FlaresolverrStatus { installed, running, proxied })
}

/// The whole opt-in unlock: install FlareSolverr if needed, start it, register
/// it in Prowlarr behind a "flaresolverr" tag, and add (or tag) the
/// Cloudflare-locked starter indexers. Returns the names newly unlocked.
#[tauri::command]
pub async fn setup_flaresolverr(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<Vec<String>> {
    use std::sync::atomic::Ordering;
    if state.flaresolverr_busy.swap(true, Ordering::SeqCst) {
        return Err(AppError::Other("FlareSolverr setup is already running".into()));
    }
    struct Busy<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for Busy<'_> {
        fn drop(&mut self) {
            self.0.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _busy = Busy(&state.flaresolverr_busy);

    // Prowlarr must be reachable BEFORE we make anyone sit through 350 MB
    let cfg = state.config.read().await.clone();
    let client = crate::commands::prowlarr_pub(&state.http, &cfg)?;
    client.ping().await.map_err(|e| {
        AppError::Other(format!("Prowlarr isn't reachable, and the unlock needs it: {e}"))
    })?;

    // someone already running FlareSolverr (their own copy) skips the
    // install entirely — :8191 answering with the real greeting is enough
    if !setup::flaresolverr_running(&state).await {
        if setup::flaresolverr_installed() {
            let started = setup::start_flaresolverr().is_ok()
                && setup::wait_for_flaresolverr(&state, 30).await;
            if !started {
                // wedged install (half-extracted, stale container, …) — replace
                setup::install_flaresolverr(&app).await?;
            }
        } else {
            setup::install_flaresolverr(&app).await?;
        }
    }

    let tag_id = client.ensure_tag("flaresolverr").await?;
    client.ensure_flaresolverr_proxy(tag_id).await?;

    // add the Cloudflare-locked starters — and TAG the ones already present,
    // whether they came from an earlier run or the user's own Prowlarr
    let installed_list = client.indexers().await?;
    let defs = client.schema().await?;
    let mut added = vec![];
    let mut already = 0usize;
    let mut last_err: Option<AppError> = None;
    for name in CF_STARTERS {
        if let Some(existing) = installed_list.iter().find(|i| i.name == *name) {
            already += 1;
            if let Ok(mut raw) = client.get_indexer_raw(existing.id).await {
                let mut tags: Vec<i64> = raw
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
                    .unwrap_or_default();
                if !tags.contains(&tag_id) {
                    tags.push(tag_id);
                    raw["tags"] = serde_json::json!(tags);
                    let _ = client.update_indexer_raw(existing.id, &raw).await;
                }
            }
            continue;
        }
        let Some(mut def) = defs
            .iter()
            .find(|d| d.get("name").and_then(|v| v.as_str()) == Some(*name))
            .cloned()
        else {
            continue;
        };
        def["enable"] = serde_json::Value::Bool(true);
        def["appProfileId"] = serde_json::Value::from(1);
        def["tags"] = serde_json::json!([tag_id]);
        match client.add_indexer_raw(&def).await {
            Ok(_) => added.push((*name).to_string()),
            Err(e) => last_err = Some(e),
        }
    }
    if added.is_empty() && already == 0 {
        if let Some(e) = last_err {
            return Err(AppError::Other(format!(
                "FlareSolverr is running, but the protected indexers still refused: {e}"
            )));
        }
    }
    {
        let conn = state.db.lock().await;
        crate::db::log_activity(
            &conn,
            "system",
            None,
            &if added.is_empty() {
                "FlareSolverr set up — protected indexers now route through it".to_string()
            } else {
                format!("FlareSolverr set up — unlocked {}", added.join(", "))
            },
        );
    }
    Ok(added)
}

/// The off-switch: opt-in must be reversible. Stops FlareSolverr, deletes the
/// managed install, and removes our proxy + tag from Prowlarr (indexers stay).
#[tauri::command]
pub async fn disable_flaresolverr(state: State<'_, AppState>) -> Result<()> {
    let cfg = state.config.read().await.clone();
    if let Ok(client) = crate::commands::prowlarr_pub(&state.http, &cfg) {
        let _ = client.remove_flaresolverr().await;
    }
    setup::remove_flaresolverr_local()?;
    {
        let conn = state.db.lock().await;
        crate::db::log_activity(&conn, "system", None, "FlareSolverr turned off and removed");
    }
    Ok(())
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
