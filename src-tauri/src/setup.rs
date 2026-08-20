//! First-run setup: detect qBittorrent / Prowlarr, install and configure what's
//! missing. Prowlarr becomes a Trawler-managed instance (user-scope, no admin);
//! qBittorrent installs via winget and gets its WebUI enabled by editing its
//! ini while it isn't running. Progress streams to the UI as `setup-step` events.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::error::{AppError, Result};
use crate::AppState;

fn emit(app: &AppHandle, component: &str, kind: &str, payload: Value) {
    let _ = app.emit(
        "setup-step",
        json!({ "component": component, "kind": kind, "payload": payload }),
    );
}

// ---------------- paths ----------------

fn local_app_data() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn managed_prowlarr_exe() -> PathBuf {
    local_app_data().join("Trawler").join("Prowlarr").join("Prowlarr.exe")
}

fn managed_prowlarr_data() -> PathBuf {
    local_app_data().join("Trawler").join("ProwlarrData")
}

fn qbt_exe_candidates() -> Vec<PathBuf> {
    vec![
        PathBuf::from(r"C:\Program Files\qBittorrent\qbittorrent.exe"),
        PathBuf::from(r"C:\Program Files (x86)\qBittorrent\qbittorrent.exe"),
    ]
}

fn qbt_ini_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("qBittorrent")
        .join("qBittorrent.ini")
}

// ---------------- detection ----------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupStatus {
    /// ok | running_no_webui | installed_stopped | missing
    pub qbit: String,
    /// ok | managed_stopped | missing
    pub prowlarr: String,
    /// ok | unreachable
    pub agent: String,
    pub prowlarr_has_indexers: bool,
}

fn process_running(name: &str) -> bool {
    let mut cmd = std::process::Command::new("tasklist");
    cmd.args(["/FI", &format!("IMAGENAME eq {name}"), "/FO", "CSV", "/NH"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(name))
        .unwrap_or(false)
}

pub async fn status(state: &AppState) -> SetupStatus {
    let cfg = state.config.read().await.clone();

    // qBittorrent: API answer is definitive
    let q = crate::qbit::QbitClient {
        http: &state.http,
        base: cfg.qbit_url.clone(),
        username: cfg.qbit_username.clone(),
        password: cfg.qbit_password.clone(),
    };
    let qbit = if q.version().await.is_ok() {
        "ok"
    } else if process_running("qbittorrent.exe") {
        "running_no_webui"
    } else if qbt_exe_candidates().iter().any(|p| p.exists()) {
        "installed_stopped"
    } else {
        "missing"
    };

    // Prowlarr
    let prowlarr_ping = state
        .http
        .get(format!("{}/ping", cfg.prowlarr_url.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(4))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    let prowlarr = if prowlarr_ping {
        "ok"
    } else if managed_prowlarr_exe().exists() {
        "managed_stopped"
    } else {
        "missing"
    };

    let mut prowlarr_has_indexers = false;
    if prowlarr_ping && !cfg.prowlarr_api_key.is_empty() {
        let client = crate::prowlarr::ProwlarrClient {
            http: &state.http,
            base: cfg.prowlarr_url.clone(),
            api_key: cfg.prowlarr_api_key.clone(),
        };
        prowlarr_has_indexers = client.indexers().await.map(|v| !v.is_empty()).unwrap_or(false);
    }

    let agent = if cfg.agent_enabled {
        let ok = crate::llm::LlmClient::new(&cfg.agent_base_url, &cfg.agent_model)
            .models()
            .await
            .is_ok();
        if ok { "ok" } else { "unreachable" }
    } else {
        "unreachable"
    };

    SetupStatus {
        qbit: qbit.into(),
        prowlarr: prowlarr.into(),
        agent: agent.into(),
        prowlarr_has_indexers,
    }
}

// ---------------- prowlarr: managed install ----------------

fn spawn_detached(exe: &PathBuf, args: &[String]) -> Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: survives Trawler exiting
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    cmd.spawn().map_err(|e| AppError::Other(format!("failed to start {}: {e}", exe.display())))?;
    Ok(())
}

pub fn start_managed_prowlarr() -> Result<()> {
    let exe = managed_prowlarr_exe();
    if !exe.exists() {
        return Err(AppError::Other("no managed Prowlarr install found".into()));
    }
    let data = managed_prowlarr_data();
    std::fs::create_dir_all(&data)?;
    spawn_detached(&exe, &[format!("-data={}", data.display())])
}

async fn wait_for_prowlarr(state: &AppState, secs: u64) -> bool {
    for _ in 0..secs {
        let ok = state
            .http
            .get("http://127.0.0.1:9696/ping")
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ok {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    false
}

fn read_prowlarr_api_key() -> Option<String> {
    let xml = std::fs::read_to_string(managed_prowlarr_data().join("config.xml")).ok()?;
    let start = xml.find("<ApiKey>")? + "<ApiKey>".len();
    let end = xml[start..].find("</ApiKey>")? + start;
    let key = xml[start..end].trim().to_string();
    if key.is_empty() { None } else { Some(key) }
}

/// Download the latest Prowlarr, extract it user-scope, start it, capture the
/// API key, and write it into Trawler's config. No admin rights involved.
pub async fn install_prowlarr(app: &AppHandle) -> Result<String> {
    let state_guard = app.state::<AppState>();
    let state: &AppState = state_guard.inner();

    emit(app, "prowlarr", "log", json!({ "message": "Finding the latest release…" }));
    let release: Value = state
        .http
        .get("https://api.github.com/repos/Prowlarr/Prowlarr/releases/latest")
        .header("User-Agent", "trawler-setup")
        .send()
        .await?
        .json()
        .await?;
    let asset_url = release
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|assets| {
            assets.iter().find_map(|a| {
                let name = a.get("name")?.as_str()?;
                if name.ends_with("windows-core-x64.zip") {
                    a.get("browser_download_url")?.as_str().map(String::from)
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| AppError::Other("could not find a Prowlarr Windows build".into()))?;

    emit(app, "prowlarr", "log", json!({ "message": "Downloading Prowlarr…" }));
    let resp = state.http.get(&asset_url).send().await?;
    let total = resp.content_length().unwrap_or(0);
    let mut bytes: Vec<u8> = Vec::with_capacity(total as usize);
    let mut stream = resp;
    let mut last_pct = 0u32;
    while let Some(chunk) = stream.chunk().await? {
        bytes.extend_from_slice(&chunk);
        if total > 0 {
            let pct = (bytes.len() as u64 * 100 / total) as u32;
            if pct >= last_pct + 5 {
                last_pct = pct;
                emit(app, "prowlarr", "progress", json!({ "pct": pct }));
            }
        }
    }

    emit(app, "prowlarr", "log", json!({ "message": "Extracting…" }));
    let target = local_app_data().join("Trawler").join("Prowlarr");
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target)?;
    let cursor = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| AppError::Other(format!("bad archive: {e}")))?;
    // the zip contains a single top-level "Prowlarr/" folder — strip it
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| AppError::Other(format!("archive read: {e}")))?;
        let Some(path) = file.enclosed_name() else { continue };
        let stripped: PathBuf = path.components().skip(1).collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let out = target.join(&stripped);
        if file.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut w = std::fs::File::create(&out)?;
            std::io::copy(&mut file, &mut w)?;
        }
    }

    emit(app, "prowlarr", "log", json!({ "message": "Starting Prowlarr…" }));
    start_managed_prowlarr()?;
    if !wait_for_prowlarr(state, 45).await {
        return Err(AppError::Other("Prowlarr installed but didn't come up on :9696".into()));
    }

    // capture the API key it generated on first boot
    let mut key = None;
    for _ in 0..15 {
        key = read_prowlarr_api_key();
        if key.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let key = key.ok_or_else(|| AppError::Other("could not read Prowlarr's API key".into()))?;

    {
        let mut cfg = state.config.write().await;
        cfg.prowlarr_url = "http://127.0.0.1:9696".into();
        cfg.prowlarr_api_key = key.clone();
        crate::config::save(&cfg)?;
    }
    emit(app, "prowlarr", "done", json!({ "message": "Prowlarr is running and connected" }));
    Ok(key)
}

// ---------------- qbittorrent ----------------

/// Idempotently force the WebUI settings Trawler needs into qBittorrent.ini.
/// Only safe while qBittorrent is NOT running (it rewrites the file on exit).
pub fn ensure_qbt_ini(ini: &str) -> String {
    let wanted = [
        (r"WebUI\Enabled", "true"),
        (r"WebUI\Port", "8080"),
        (r"WebUI\LocalHostAuth", "false"),
    ];
    let mut lines: Vec<String> = ini.lines().map(String::from).collect();
    let mut in_prefs = false;
    let mut prefs_end = lines.len();
    let mut seen: Vec<&str> = vec![];
    let mut found_section = false;

    for i in 0..lines.len() {
        let line = lines[i].trim().to_string();
        if line.starts_with('[') {
            if in_prefs {
                prefs_end = i;
            }
            in_prefs = line == "[Preferences]";
            if in_prefs {
                found_section = true;
                prefs_end = lines.len();
            }
            continue;
        }
        if in_prefs {
            for (k, v) in wanted {
                if line.starts_with(&format!("{k}=")) {
                    lines[i] = format!("{k}={v}");
                    seen.push(k);
                }
            }
        }
    }

    if !found_section {
        lines.push("[Preferences]".into());
        prefs_end = lines.len();
    }
    for (k, v) in wanted {
        if !seen.contains(&k) {
            lines.insert(prefs_end, format!("{k}={v}"));
            prefs_end += 1;
        }
    }
    let mut out = lines.join("\r\n");
    out.push_str("\r\n");
    out
}

/// Enable the WebUI (file edit) and launch qBittorrent. Requires it stopped.
pub fn configure_and_launch_qbt() -> Result<()> {
    if process_running("qbittorrent.exe") {
        return Err(AppError::Other(
            "qBittorrent is running — close it first so its settings can be updated".into(),
        ));
    }
    let exe = qbt_exe_candidates()
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| AppError::Other("qBittorrent doesn't appear to be installed".into()))?;

    let ini_path = qbt_ini_path();
    let current = std::fs::read_to_string(&ini_path).unwrap_or_default();
    let updated = ensure_qbt_ini(&current);
    if let Some(parent) = ini_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&ini_path, updated)?;

    spawn_detached(&exe, &[])
}

/// Kick a winget install of qBittorrent (fires one UAC prompt); the caller
/// polls status until the exe appears, then runs configure_and_launch.
pub fn install_qbt_via_winget() -> Result<()> {
    let mut cmd = std::process::Command::new("winget");
    cmd.args([
        "install", "--id", "qBittorrent.qBittorrent", "-e",
        "--accept-source-agreements", "--accept-package-agreements", "--silent",
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — UAC still prompts
    }
    cmd.spawn()
        .map_err(|_| AppError::Other("winget isn't available — install qBittorrent from qbittorrent.org, then return here".into()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ensure_qbt_ini;

    #[test]
    fn creates_missing_ini() {
        let out = ensure_qbt_ini("");
        assert!(out.contains("[Preferences]"));
        assert!(out.contains(r"WebUI\Enabled=true"));
        assert!(out.contains(r"WebUI\Port=8080"));
        assert!(out.contains(r"WebUI\LocalHostAuth=false"));
    }

    #[test]
    fn updates_existing_values_in_place() {
        let ini = "[BitTorrent]\r\nSession\\Port=23177\r\n[Preferences]\r\nWebUI\\Enabled=false\r\nWebUI\\Port=9090\r\nGeneral\\Locale=en\r\n";
        let out = ensure_qbt_ini(ini);
        assert!(out.contains(r"WebUI\Enabled=true"));
        assert!(out.contains(r"WebUI\Port=8080"));
        assert!(!out.contains(r"WebUI\Port=9090"));
        assert!(out.contains(r"WebUI\LocalHostAuth=false")); // appended
        assert!(out.contains(r"Session\Port=23177")); // untouched
        assert!(out.contains(r"General\Locale=en"));
    }

    #[test]
    fn respects_section_boundaries() {
        // keys must land inside [Preferences], not in a later section
        let ini = "[Preferences]\r\nGeneral\\Locale=en\r\n[RSS]\r\nFeeds=none\r\n";
        let out = ensure_qbt_ini(ini);
        let prefs_pos = out.find("[Preferences]").unwrap();
        let rss_pos = out.find("[RSS]").unwrap();
        let enabled_pos = out.find(r"WebUI\Enabled=true").unwrap();
        assert!(enabled_pos > prefs_pos && enabled_pos < rss_pos);
    }
}
