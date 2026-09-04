mod agent_run;
mod applog;
mod agent_tools;
mod briefs;
mod commands;
mod commands_agent;
mod commands_discover;
mod commands_import;
mod commands_setup;
mod config;
mod db;
mod error;
mod follows;
mod bitport;
mod grab;
mod launch_visibility;
mod llm;
mod notify;
mod parse;
mod prowlarr;
mod qbit;
mod rss;
mod scheduler;
mod setup;
mod tray;
mod tvmaze;
mod upgrade;

use std::sync::atomic::AtomicBool;
use tokio::sync::{Mutex, RwLock};

pub struct AppState {
    pub http: reqwest::Client,
    pub config: RwLock<config::Config>,
    pub db: Mutex<rusqlite::Connection>,
    /// guards against overlapping scheduler cycles
    pub scheduler_busy: AtomicBool,
    /// one chat run at a time
    pub agent_chat_busy: AtomicBool,
    /// one brief tick at a time
    pub brief_tick_busy: AtomicBool,
    /// one RSS sweep at a time
    pub rss_busy: AtomicBool,
    /// one upgrade-scout pass at a time
    pub scout_busy: AtomicBool,
    /// one FlareSolverr install/setup at a time
    pub flaresolverr_busy: AtomicBool,
    /// one managed Prowlarr install/start operation at a time
    pub prowlarr_busy: AtomicBool,
    /// content keys with a grab in flight (see grab::dispatch)
    pub grab_claims: std::sync::Arc<grab::GrabClaims>,
    /// (fetched_at, payload) for the discovery rows
    pub discover_cache: Mutex<Option<(i64, serde_json::Value)>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must be first: a second process would otherwise create independent
        // scheduler/config state and can duplicate work. Bring the existing
        // instance forward instead.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_main(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        // (the updater exits the process on Windows after handing off to the
        // installer; sqlite WAL is crash-safe and every background loop is
        // idempotent, so no explicit teardown is required here)
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            use tauri::Manager;
            applog::attach(app.handle());
            applog::info("app", format!("Trawler {} starting", env!("CARGO_PKG_VERSION")));
            // The database opens HERE, after the single-instance plugin has
            // run: a second process exits up there without ever touching the
            // file. A corrupt file (and only a corrupt one - a busy or locked
            // database is healthy) is moved aside and a fresh one created,
            // as config.rs does for config.json; the old abort-before-any-
            // window behaviour made Trawler look uninstalled under autostart.
            let (conn, kept) = db::open_or_recover().inspect_err(|error| {
                applog::error("app", format!("trawler.db could not be opened: {error}"));
            })?;
            if let Some(kept) = kept {
                applog::error(
                    "app",
                    format!(
                        "trawler.db was unreadable — it was moved to {} and Trawler started on a fresh database",
                        kept.display()
                    ),
                );
            }
            let http = reqwest::Client::builder()
                .cookie_store(true)
                .user_agent(format!("trawler/{}", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(60))
                .build()?;
            app.manage(AppState {
                http,
                config: RwLock::new(config::load()),
                db: Mutex::new(conn),
                scheduler_busy: AtomicBool::new(false),
                agent_chat_busy: AtomicBool::new(false),
                brief_tick_busy: AtomicBool::new(false),
                rss_busy: AtomicBool::new(false),
                scout_busy: AtomicBool::new(false),
                flaresolverr_busy: AtomicBool::new(false),
                prowlarr_busy: AtomicBool::new(false),
                grab_claims: std::sync::Arc::new(grab::GrabClaims::default()),
                discover_cache: Mutex::new(None),
            });
            // a Trawler-managed Prowlarr should come back after reboots
            let boot_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                use tauri::Manager;
                let state = boot_handle.state::<AppState>();
                let url = { state.config.read().await.prowlarr_url.clone() };
                if url.contains("127.0.0.1:9696")
                    && setup::managed_prowlarr_install_is_complete()
                {
                    let up = state
                        .http
                        .get("http://127.0.0.1:9696/ping")
                        .timeout(std::time::Duration::from_secs(3))
                        .send()
                        .await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false);
                    if !up {
                        let _ = setup::start_managed_prowlarr(state.inner()).await;
                    }
                } else if url.contains("127.0.0.1:9696")
                    && setup::managed_prowlarr_exe().exists()
                {
                    applog::warn(
                        "setup",
                        "managed Prowlarr installation is incomplete — waiting for a clean reinstall",
                    );
                }
                // FlareSolverr is opt-in; but once opted in, it should come
                // back after a reboot without the user thinking about it
                if setup::flaresolverr_installed() && !setup::flaresolverr_running(&state).await {
                    let _ = setup::start_flaresolverr();
                }
            });
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(scheduler::scheduler_loop(handle));
            let rss_handle = app.handle().clone();
            tauri::async_runtime::spawn(rss::rss_loop(rss_handle));
            let scout_handle = app.handle().clone();
            tauri::async_runtime::spawn(upgrade::scout_loop(scout_handle));
            let brief_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                use std::sync::atomic::Ordering;
                use tauri::Manager;
                tokio::time::sleep(std::time::Duration::from_secs(45)).await;
                loop {
                    {
                        let state = brief_handle.state::<AppState>();
                        if !state.brief_tick_busy.swap(true, Ordering::SeqCst) {
                            // drop guard + inner spawn: a panicking tick must
                            // neither wedge the flag nor kill this loop. In
                            // dev builds the guard releases during unwind and
                            // the loop continues; release builds abort on
                            // panic, so the process dies and the flag resets
                            // with it — either way it can't wedge silently.
                            struct Busy<'a>(&'a std::sync::atomic::AtomicBool);
                            impl Drop for Busy<'_> {
                                fn drop(&mut self) {
                                    self.0.store(false, Ordering::SeqCst);
                                }
                            }
                            let _busy = Busy(&state.brief_tick_busy);
                            let h = brief_handle.clone();
                            let tick = tauri::async_runtime::spawn(async move { briefs::tick(&h).await });
                            if let Err(e) = tick.await {
                                applog::error("briefs", format!("brief tick died: {e} — continuing next minute"));
                            }
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
            });
            tray::init(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                use tauri::Manager;
                let state = window.app_handle().state::<AppState>();
                let close_to_tray = state
                    .config
                    .try_read()
                    .map(|c| c.close_to_tray)
                    .unwrap_or(true);
                // hiding into a tray that doesn't exist (icon resource
                // missing) would leave an unopenable zombie — close for real
                let has_tray = window.app_handle().tray_by_id("main").is_some();
                if close_to_tray && has_tray {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_config,
            commands::test_connections,
            commands::list_indexers,
            commands::search,
            commands::grab,
            commands::downloads,
            commands::torrent_action,
            commands::search_tvmaze,
            commands::follow_show,
            commands::unfollow_show,
            commands::get_shows,
            commands::get_show_episodes,
            commands::refresh_show,
            commands::set_episode_state,
            commands::set_show_options,
            commands::get_activity,
            commands::preview_show_grabs,
            commands::run_scheduler_now,
            commands::indexer_defs,
            commands::add_indexer,
            commands::toggle_indexer,
            commands::remove_indexer,
            commands::get_autostart,
            commands::set_autostart,
            commands::notify_test,
            commands::upgrade_scan_now,
            commands_agent::agent_send,
            commands_agent::agent_history,
            commands_agent::agent_clear,
            commands_agent::agent_models,
            commands_agent::briefs_list,
            commands_agent::compile_brief,
            commands_agent::brief_save,
            commands_agent::brief_delete,
            commands_agent::brief_run_now,
            commands_agent::proposals_list,
            commands_agent::proposal_resolve,
            commands_setup::setup_status,
            commands_setup::setup_install_prowlarr,
            commands_setup::setup_start_prowlarr,
            commands_setup::setup_install_qbit,
            commands_setup::setup_configure_qbit,
            commands_setup::setup_starter_indexers,
            commands_setup::flaresolverr_status,
            commands_setup::setup_flaresolverr,
            commands_setup::disable_flaresolverr,
            commands_setup::setup_save_prowlarr_key,
            commands_setup::setup_finish,
            commands_setup::rss_sweep_now,
            commands::bitport_authorize_url,
            commands::bitport_connect_flow,
            commands::bitport_connect,
            commands::bitport_status,
            commands::bitport_disconnect,
            commands::bitport_delete,
            commands::logs_recent,
            commands::logs_support_bundle,
            commands_discover::calendar_range,
            commands_discover::export_ical,
            commands_discover::discover,
            commands_discover::show_preview,
            commands_import::import_lookup,
            commands_import::import_estimate,
            commands_import::import_disambiguate,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // The NSIS updater can relaunch a healthy WebView without a
            // visible top-level window. Force a visible launch once per new
            // version; same-version tray and autostart behavior is unchanged.
            #[cfg(target_os = "windows")]
            if matches!(&event, tauri::RunEvent::Ready) {
                if launch_visibility::should_show() && tray::show_main(app_handle) {
                    if let Err(e) = launch_visibility::record_visible() {
                        applog::warn("app", format!("could not record visible launch version: {e}"));
                    }
                }
            }
            // clicking the Dock icon with the window hidden is THE macOS
            // gesture for "bring it back" — without this it does nothing
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                tray::show_main(app_handle);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app_handle, &event);
        });
}
