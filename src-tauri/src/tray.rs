//! System tray: Trawler lives here when the window is closed, so the
//! follow scheduler keeps running.

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{TrayIconBuilder, TrayIconEvent},
    App, AppHandle, Manager,
};

pub(crate) fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

pub fn init(app: &App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Trawler", true, None::<&str>)?;
    let check = MenuItem::with_id(app, "check", "Check for episodes now", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Trawler", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &check, &sep, &quit])?;

    // a missing icon resource must degrade to a tray-less run, not abort
    // startup inside setup
    let Some(icon) = app.default_window_icon() else {
        crate::applog::warn("app", "default window icon missing — running without a tray".to_string());
        return Ok(());
    };

    TrayIconBuilder::with_id("main")
        .icon(icon.clone())
        .tooltip("Trawler")
        .menu(&menu)
        // macOS status items open their menu on left-click — that's the norm;
        // on Windows left-click is reserved and double-click opens the app
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "check" => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    match crate::scheduler::run_cycle(&handle).await {
                        Ok(n) => crate::applog::info("app",format!("tray-triggered cycle: {n} grabs")),
                        Err(e) => crate::applog::warn("app",format!("tray-triggered cycle failed: {e}")),
                    }
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::DoubleClick { .. } = event {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}
