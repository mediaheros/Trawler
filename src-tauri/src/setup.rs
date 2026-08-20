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

/// Managed Prowlarr lives OUTSIDE the app install dir (the NSIS installer
/// owns %LOCALAPPDATA%\Trawler for Trawler itself — updates and uninstalls
/// operate on that folder). Legacy installs that landed inside it keep working.
pub fn managed_prowlarr_exe() -> PathBuf {
    let new = local_app_data().join("TrawlerTools").join("Prowlarr").join("Prowlarr.exe");
    if new.exists() {
        return new;
    }
    let legacy = local_app_data().join("Trawler").join("Prowlarr").join("Prowlarr.exe");
    if legacy.exists() { legacy } else { new }
}

fn managed_prowlarr_dir() -> PathBuf {
    managed_prowlarr_exe()
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| local_app_data().join("TrawlerTools").join("Prowlarr"))
}

fn managed_prowlarr_data() -> PathBuf {
    let legacy = local_app_data().join("Trawler").join("ProwlarrData");
    if legacy.join("config.xml").exists() {
        return legacy;
    }
    local_app_data().join("TrawlerTools").join("ProwlarrData")
}

fn qbt_exe_candidates() -> Vec<PathBuf> {
    let mut out = vec![];
    // honor real environment paths, not a hardcoded C: drive
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "LOCALAPPDATA"] {
        if let Ok(base) = std::env::var(var) {
            out.push(PathBuf::from(base).join("qBittorrent").join("qbittorrent.exe"));
        }
    }
    out.push(PathBuf::from(r"C:\Program Files\qBittorrent\qbittorrent.exe"));
    out.push(PathBuf::from(r"C:\Program Files (x86)\qBittorrent\qbittorrent.exe"));
    // last resort: ask Windows where the running/registered copy lives
    if let Some(p) = qbt_exe_from_registry() {
        out.insert(0, p);
    }
    out.dedup();
    out
}

/// InstallLocation from qBittorrent's uninstall key (HKCU then HKLM).
fn qbt_exe_from_registry() -> Option<PathBuf> {
    for root in ["HKCU", "HKLM"] {
        let mut cmd = std::process::Command::new("reg");
        cmd.args([
            "query",
            &format!(r"{root}\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\qBittorrent"),
            "/v",
            "InstallLocation",
        ]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        if let Ok(out) = cmd.output() {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = text.lines().find(|l| l.contains("InstallLocation")) {
                if let Some(idx) = line.find("REG_SZ") {
                    let dir = line[idx + 6..].trim();
                    if !dir.is_empty() {
                        let exe = PathBuf::from(dir).join("qbittorrent.exe");
                        if exe.exists() {
                            return Some(exe);
                        }
                    }
                }
            }
        }
    }
    None
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
    /// ok | needs_key | managed_stopped | missing
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

    // Prowlarr. /ping is UNAUTHENTICATED — a green ping alone must never
    // report "connected", or every authenticated call afterwards fails
    // while the wizard shows an all-clear.
    let prowlarr_ping = state
        .http
        .get(format!("{}/ping", cfg.prowlarr_url.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(4))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);

    // self-heal a managed install whose key never made it into our config
    let mut api_key = cfg.prowlarr_api_key.clone();
    if prowlarr_ping && api_key.is_empty() && cfg.prowlarr_url.contains("127.0.0.1:9696") {
        if let Some(found) = read_prowlarr_api_key() {
            let mut w = state.config.write().await;
            w.prowlarr_api_key = found.clone();
            let _ = crate::config::save(&w);
            api_key = found;
        }
    }

    let mut prowlarr_has_indexers = false;
    let prowlarr = if prowlarr_ping {
        if api_key.is_empty() {
            "needs_key"
        } else {
            let client = crate::prowlarr::ProwlarrClient {
                http: &state.http,
                base: cfg.prowlarr_url.clone(),
                api_key,
            };
            match client.indexers().await {
                Ok(v) => {
                    prowlarr_has_indexers = !v.is_empty();
                    "ok"
                }
                // the key is present but Prowlarr rejects it
                Err(crate::error::AppError::Prowlarr { status: 401, .. }) => "needs_key",
                // transient hiccup: still connected, just no indexer info yet
                Err(_) => "ok",
            }
        }
    } else if managed_prowlarr_exe().exists() {
        "managed_stopped"
    } else {
        "missing"
    };

    let agent = if cfg.agent_enabled {
        let client = crate::llm::LlmClient::new(&cfg.agent_base_url, &cfg.agent_model);
        match tokio::time::timeout(std::time::Duration::from_secs(4), client.models()).await {
            Ok(Ok(_)) => "ok",
            _ => "unreachable",
        }
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

/// 32 hex chars from OS entropy (RandomState seeds from the OS CSPRNG).
fn random_api_key() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(32);
    for i in 0..2u8 {
        let mut h = RandomState::new().build_hasher();
        h.write_u8(i);
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// Replace a flat <Tag>value</Tag> under <Config>, or insert it if missing.
pub fn upsert_xml_tag(xml: &str, tag: &str, value: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    if let (Some(s), Some(e)) = (xml.find(&open), xml.find(&close)) {
        if s < e {
            return format!("{}{}{}{}", &xml[..s + open.len()], value, close, &xml[e + close.len()..]);
        }
    }
    match xml.rfind("</Config>") {
        Some(pos) => format!("{}  {open}{value}{close}
{}", &xml[..pos], &xml[pos..]),
        None => format!("<Config>
  {open}{value}{close}
</Config>
"),
    }
}

/// A Trawler-managed Prowlarr must never pop a browser or demand an admin
/// login: localhost-only, local auth exempt, browser launch off. Seeded
/// BEFORE first boot so the auth-setup form never exists; re-applied gently
/// on later boots — a login the user deliberately created is left alone.
fn seed_managed_prowlarr_config() -> Result<String> {
    let data = managed_prowlarr_data();
    std::fs::create_dir_all(&data)?;
    let path = data.join("config.xml");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let key = read_prowlarr_api_key().unwrap_or_else(random_api_key);

    let mut xml = if existing.trim().is_empty() {
        "<Config>
</Config>
".to_string()
    } else {
        existing
    };
    xml = upsert_xml_tag(&xml, "BindAddress", "127.0.0.1");
    xml = upsert_xml_tag(&xml, "Port", "9696");
    xml = upsert_xml_tag(&xml, "ApiKey", &key);
    xml = upsert_xml_tag(&xml, "LaunchBrowser", "False");
    let has_user_auth = xml.contains("<AuthenticationMethod>Forms</AuthenticationMethod>")
        || xml.contains("<AuthenticationMethod>Basic</AuthenticationMethod>");
    if !has_user_auth {
        xml = upsert_xml_tag(&xml, "AuthenticationMethod", "External");
        xml = upsert_xml_tag(&xml, "AuthenticationRequired", "DisabledForLocalAddresses");
    }
    std::fs::write(&path, xml)?;
    Ok(key)
}

pub fn start_managed_prowlarr() -> Result<()> {
    let exe = managed_prowlarr_exe();
    if !exe.exists() {
        return Err(AppError::Other("no managed Prowlarr install found".into()));
    }
    let _ = seed_managed_prowlarr_config();
    let data = managed_prowlarr_data();
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
    let api = state
        .http
        .get("https://api.github.com/repos/Prowlarr/Prowlarr/releases/latest")
        .header("User-Agent", "trawler-setup")
        .send()
        .await?;
    if api.status().as_u16() == 403 {
        return Err(AppError::Other(
            "GitHub is rate-limiting release lookups from this network — try again in a few minutes".into(),
        ));
    }
    let release: Value = api.error_for_status()?.json().await?;
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
    // ~100 MB: the client's 60s TOTAL deadline would abort this on any
    // connection under ~15 Mbps, mid-wizard, with a confusing error
    let resp = state
        .http
        .get(&asset_url)
        .timeout(std::time::Duration::from_secs(1800))
        .send()
        .await?
        .error_for_status()
        .map_err(|e| AppError::Other(format!("Prowlarr download failed: {e}")))?;
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
    let target = managed_prowlarr_dir();
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

    // Seed config BEFORE first boot: our own API key, localhost-only, no
    // browser popup, no admin-login form. First impressions matter.
    emit(app, "prowlarr", "log", json!({ "message": "Configuring…" }));
    let key = seed_managed_prowlarr_config()?;

    emit(app, "prowlarr", "log", json!({ "message": "Starting Prowlarr…" }));
    start_managed_prowlarr()?;
    if !wait_for_prowlarr(state, 45).await {
        return Err(AppError::Other("Prowlarr installed but didn't come up on :9696".into()));
    }

    {
        let mut cfg = state.config.write().await;
        cfg.prowlarr_url = "http://127.0.0.1:9696".into();
        cfg.prowlarr_api_key = key.clone();
        crate::config::save(&cfg)?;
    }
    emit(app, "prowlarr", "done", json!({ "message": "Prowlarr is running and connected" }));
    Ok(key)
}

// ---------------- flaresolverr (optional) ----------------
// FlareSolverr is strictly opt-in: the stack runs fine without it, it only
// unlocks Cloudflare-protected indexers (1337x, EZTV, …) for users who ask.

pub fn managed_flaresolverr_exe() -> PathBuf {
    local_app_data().join("TrawlerTools").join("FlareSolverr").join("flaresolverr.exe")
}

pub async fn flaresolverr_running(state: &AppState) -> bool {
    // require FlareSolverr's own greeting — "anything 2xx on :8191" would let
    // an unrelated service masquerade as a working solver
    let Ok(resp) = state
        .http
        .get("http://127.0.0.1:8191/")
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    else {
        return false;
    };
    if !resp.status().is_success() {
        return false;
    }
    resp.text().await.map(|t| t.contains("FlareSolverr")).unwrap_or(false)
}

pub fn start_flaresolverr() -> Result<()> {
    let exe = managed_flaresolverr_exe();
    if !exe.exists() {
        return Err(AppError::Other("FlareSolverr isn't installed".into()));
    }
    spawn_detached(&exe, &[])
}

pub async fn wait_for_flaresolverr(state: &AppState, secs: u64) -> bool {
    for _ in 0..secs {
        if flaresolverr_running(state).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    false
}

/// Download the latest FlareSolverr (~350 MB — it bundles a browser), extract
/// it next to managed Prowlarr, and start it. Opt-in only; never part of the
/// wizard.
pub async fn install_flaresolverr(app: &AppHandle) -> Result<()> {
    let state_guard = app.state::<AppState>();
    let state: &AppState = state_guard.inner();

    emit(app, "flaresolverr", "log", json!({ "message": "Finding the latest release…" }));
    let api = state
        .http
        .get("https://api.github.com/repos/FlareSolverr/FlareSolverr/releases/latest")
        .header("User-Agent", "trawler-setup")
        .send()
        .await?;
    if api.status().as_u16() == 403 {
        return Err(AppError::Other(
            "GitHub is rate-limiting release lookups from this network — try again in a few minutes".into(),
        ));
    }
    let release: Value = api.error_for_status()?.json().await?;
    let asset_url = release
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|assets| {
            assets.iter().find_map(|a| {
                let name = a.get("name")?.as_str()?;
                if name == "flaresolverr_windows_x64.zip" {
                    a.get("browser_download_url")?.as_str().map(String::from)
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| AppError::Other("could not find a FlareSolverr Windows build".into()))?;
    // the URL came out of an API response — only fetch a real GitHub asset host
    if !(asset_url.starts_with("https://github.com/FlareSolverr/")
        || asset_url.starts_with("https://objects.githubusercontent.com/"))
    {
        return Err(AppError::Other("FlareSolverr release URL looked wrong — not downloading it".into()));
    }

    emit(app, "flaresolverr", "log", json!({ "message": "Downloading FlareSolverr (~350 MB — it bundles a browser)…" }));
    let resp = state
        .http
        .get(&asset_url)
        .timeout(std::time::Duration::from_secs(3600))
        .send()
        .await?
        .error_for_status()
        .map_err(|e| AppError::Other(format!("FlareSolverr download failed: {e}")))?;
    let total = resp.content_length().unwrap_or(0);

    // ~350 MB zip + ~700 MB unpacked: stream to disk, never into RAM
    let tools = local_app_data().join("TrawlerTools");
    std::fs::create_dir_all(&tools)?;
    let zip_path = tools.join("flaresolverr-download.zip");
    // the 326 MB temp zip must not survive ANY failure path
    struct TempFile(PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _zip_guard = TempFile(zip_path.clone());
    let mut downloaded: u64 = 0;
    {
        let mut out = std::fs::File::create(&zip_path)?;
        let mut stream = resp;
        let mut got: u64 = 0;
        let mut last_pct = 0u32;
        while let Some(chunk) = stream.chunk().await? {
            use std::io::Write;
            out.write_all(&chunk)?;
            got += chunk.len() as u64;
            if total > 0 {
                let pct = (got * 100 / total) as u32;
                if pct >= last_pct + 5 {
                    last_pct = pct;
                    emit(app, "flaresolverr", "progress", json!({ "pct": pct }));
                }
            }
        }
        downloaded = got;
    }
    if total > 0 && downloaded != total {
        return Err(AppError::Other(format!(
            "the download ended early ({downloaded} of {total} bytes) — check the connection and try again"
        )));
    }

    emit(app, "flaresolverr", "log", json!({ "message": "Extracting…" }));
    // extract into a staging dir and rename on success — a half-written
    // install must never look installed to the status probe or the boot hook
    let target = tools.join("FlareSolverr");
    let staging = tools.join("FlareSolverr.new");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let staging_guard = TempDir(staging.clone());
    {
        let f = std::fs::File::open(&zip_path)?;
        let mut archive =
            zip::ZipArchive::new(f).map_err(|e| AppError::Other(format!("bad archive: {e}")))?;
        // single top-level "flaresolverr/" folder — strip it
        for i in 0..archive.len() {
            let mut file = archive
                .by_index(i)
                .map_err(|e| AppError::Other(format!("archive read: {e}")))?;
            let Some(path) = file.enclosed_name() else { continue };
            let stripped: PathBuf = path.components().skip(1).collect();
            if stripped.as_os_str().is_empty() {
                continue;
            }
            let out = staging.join(&stripped);
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
    }
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&staging, &target)?;
    std::mem::forget(staging_guard);

    emit(app, "flaresolverr", "log", json!({ "message": "Starting FlareSolverr…" }));
    start_flaresolverr()?;
    if !wait_for_flaresolverr(state, 60).await {
        return Err(AppError::Other("FlareSolverr installed but didn't come up on :8191".into()));
    }
    Ok(())
}

// ---------------- qbittorrent ----------------

/// qBittorrent's password blob: PBKDF2-HMAC-SHA512, 100k rounds,
/// base64(salt):base64(dk). qBittorrent 5.x REFUSES to start the WebUI when
/// no credentials exist — even with localhost auth bypassed — so a fresh
/// install must be seeded with a (random, never-shown) password.
fn qbt_password_blob(password: &str) -> String {
    use base64::Engine;
    use pbkdf2::pbkdf2_hmac;
    use sha2::Sha512;
    let salt: [u8; 16] = {
        let mut s = [0u8; 16];
        let hex = random_api_key(); // 32 hex chars of OS entropy
        for (i, b) in s.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
        }
        s
    };
    let mut dk = [0u8; 64];
    pbkdf2_hmac::<Sha512>(password.as_bytes(), &salt, 100_000, &mut dk);
    let b64 = base64::engine::general_purpose::STANDARD;
    format!("@ByteArray({}:{})", b64.encode(salt), b64.encode(dk))
}

/// Idempotently force the WebUI settings Trawler needs into qBittorrent.ini.
/// Only safe while qBittorrent is NOT running (it rewrites the file on exit).
/// `fresh` additionally pre-accepts the legal notice (only used when the ini
/// didn't exist — i.e. the install the user explicitly clicked for; without
/// it qBittorrent halts ALL startup, WebUI included, behind a modal).
pub fn ensure_qbt_ini(ini: &str, credentials: Option<(&str, &str)>, fresh: bool) -> String {
    ensure_qbt_ini_port(ini, credentials, fresh, 8080)
}

/// Same, but honoring a caller-supplied WebUI port (from cfg.qbit_url) —
/// forcing 8080 onto someone who runs the WebUI on 9090 broke their setup.
pub fn ensure_qbt_ini_port(ini: &str, credentials: Option<(&str, &str)>, fresh: bool, port: u16) -> String {
    let mut wanted: Vec<(String, String)> = vec![
        (r"WebUI\Enabled".into(), "true".into()),
        (r"WebUI\Port".into(), port.to_string()),
        (r"WebUI\LocalHostAuth".into(), "false".into()),
    ];
    // never clobber a password the user already has
    if !ini.contains(r"WebUI\Password_PBKDF2") {
        if let Some((user, blob)) = credentials {
            wanted.push((r"WebUI\Username".into(), user.into()));
            wanted.push((r"WebUI\Password_PBKDF2".into(), format!("\"{blob}\"")));
        }
    }
    let wanted: Vec<(&str, String)> = wanted.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
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
            for (k, v) in &wanted {
                if line.starts_with(&format!("{k}=")) {
                    lines[i] = format!("{k}={v}");
                    seen.push(*k);
                }
            }
        }
    }

    if !found_section {
        lines.push("[Preferences]".into());
        prefs_end = lines.len();
    }
    for (k, v) in &wanted {
        if !seen.contains(k) {
            lines.insert(prefs_end, format!("{k}={v}"));
            prefs_end += 1;
        }
    }
    // a truly fresh install: pre-accept the notice the user's install click
    // already implied, or qBittorrent freezes ALL of startup behind a modal
    if fresh && !lines.iter().any(|l| l.trim() == "[LegalNotice]") {
        lines.insert(0, String::new());
        lines.insert(0, "Accepted=true".into());
        lines.insert(0, "[LegalNotice]".into());
    }
    let mut out = lines.join("\r\n");
    out.push_str("\r\n");
    out
}

/// Enable the WebUI and (re)launch qBittorrent. If it's running we close it
/// ourselves — its X button only hides to tray, so "close it first" was a
/// trap users couldn't escape.
pub async fn configure_and_launch_qbt(qbit_url: &str) -> Result<()> {
    let qbit_port: u16 = url::Url::parse(qbit_url)
        .ok()
        .and_then(|u| u.port_or_known_default())
        .unwrap_or(8080);
    // resolve BEFORE killing anything: a custom install path used to mean we
    // force-closed the user's client and then failed to relaunch it
    let exe = qbt_exe_candidates()
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| AppError::Other(
            "couldn't find qBittorrent's program folder — install it from qbittorrent.org, or set its Web UI up manually in Settings".into(),
        ))?;
    if process_running("qbittorrent.exe") {
        let mut kill = std::process::Command::new("taskkill");
        kill.args(["/IM", "qbittorrent.exe", "/F"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            kill.creation_flags(0x0800_0000);
        }
        let _ = kill.output();
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            if !process_running("qbittorrent.exe") {
                break;
            }
        }
        if process_running("qbittorrent.exe") {
            return Err(AppError::Other("couldn't close qBittorrent — try quitting it from its tray icon".into()));
        }
    }
    let ini_path = qbt_ini_path();
    let (current, fresh) = match std::fs::read_to_string(&ini_path) {
        Ok(s) => {
            let fresh = s.trim().is_empty();
            (s, fresh)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), true),
        Err(e) => {
            return Err(AppError::Other(format!(
                "couldn't read qBittorrent's settings at {} ({e}) — close qBittorrent and try again",
                ini_path.display()
            )))
        }
    };
    // random password nobody ever needs to see — localhost API access is
    // exempted from auth; qBittorrent just requires that credentials EXIST
    let password = random_api_key();
    let blob = qbt_password_blob(&password);
    let updated = ensure_qbt_ini_port(&current, Some(("admin", &blob)), fresh, qbit_port);
    if let Some(parent) = ini_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&ini_path, updated)?;

    spawn_detached(&exe, &[])
}

/// Install qBittorrent via winget and babysit it: stream status while the
/// download / UAC / install runs, return once the exe actually exists. The
/// caller's button stays busy the whole time instead of going quiet.
pub async fn install_qbt_via_winget(app: &AppHandle) -> Result<()> {
    if qbt_exe_candidates().iter().any(|p| p.exists()) {
        return Ok(());
    }
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

    emit(app, "qbit", "log", json!({ "message": "Downloading via winget — approve the Windows prompt if one appears…" }));
    for i in 0..180u32 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if qbt_exe_candidates().iter().any(|p| p.exists()) {
            emit(app, "qbit", "done", json!({ "message": "qBittorrent installed" }));
            return Ok(());
        }
        if i == 45 {
            emit(app, "qbit", "log", json!({ "message": "Still installing — winget can take a few minutes…" }));
        }
    }
    Err(AppError::Other(
        "the installer didn't finish within 6 minutes — if you dismissed the Windows prompt, click Install again".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::ensure_qbt_ini;
    use super::upsert_xml_tag;

    #[test]
    fn xml_upsert_replaces_and_inserts() {
        let xml = "<Config>\n  <Port>9696</Port>\n  <LaunchBrowser>True</LaunchBrowser>\n</Config>\n";
        let out = upsert_xml_tag(xml, "LaunchBrowser", "False");
        assert!(out.contains("<LaunchBrowser>False</LaunchBrowser>"));
        assert!(!out.contains("<LaunchBrowser>True</LaunchBrowser>"));
        // untouched sibling survives
        assert!(out.contains("<Port>9696</Port>"));
        // missing tag gets inserted inside <Config>
        let out2 = upsert_xml_tag(&out, "BindAddress", "127.0.0.1");
        assert!(out2.contains("<BindAddress>127.0.0.1</BindAddress>"));
        assert!(out2.rfind("</Config>").unwrap() > out2.find("<BindAddress>").unwrap());
        // degenerate input still produces a valid config
        let out3 = upsert_xml_tag("", "ApiKey", "abc");
        assert!(out3.contains("<Config>") && out3.contains("<ApiKey>abc</ApiKey>"));
    }

    #[test]
    fn api_keys_are_32_hex_and_unique() {
        let a = super::random_api_key();
        let b = super::random_api_key();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn creates_missing_ini() {
        let out = ensure_qbt_ini("", None, false);
        assert!(out.contains("[Preferences]"));
        assert!(out.contains(r"WebUI\Enabled=true"));
        assert!(out.contains(r"WebUI\Port=8080"));
        assert!(out.contains(r"WebUI\LocalHostAuth=false"));
    }

    #[test]
    fn updates_existing_values_in_place() {
        let ini = "[BitTorrent]\r\nSession\\Port=23177\r\n[Preferences]\r\nWebUI\\Enabled=false\r\nWebUI\\Port=9090\r\nGeneral\\Locale=en\r\n";
        let out = ensure_qbt_ini(ini, None, false);
        assert!(out.contains(r"WebUI\Enabled=true"));
        assert!(out.contains(r"WebUI\Port=8080"));
        assert!(!out.contains(r"WebUI\Port=9090"));
        assert!(out.contains(r"WebUI\LocalHostAuth=false")); // appended
        assert!(out.contains(r"Session\Port=23177")); // untouched
        assert!(out.contains(r"General\Locale=en"));
    }

    #[test]
    fn seeds_credentials_and_legal_notice() {
        let blob = "@ByteArray(abc:def)";
        let out = ensure_qbt_ini("", Some(("admin", blob)), true);
        assert!(out.contains("[LegalNotice]"));
        assert!(out.contains("Accepted=true"));
        assert!(out.contains("Password_PBKDF2"));
        assert!(out.contains("@ByteArray(abc:def)"));
        // an existing password is sacred
        let existing = concat!("[Preferences]", "\r\n", r"WebUI\Password_PBKDF2=USER_OWN", "\r\n");
        let out2 = ensure_qbt_ini(existing, Some(("admin", blob)), false);
        assert!(out2.contains("USER_OWN"));
        assert!(!out2.contains("abc:def"));
        // not fresh: no legal notice injected
        assert!(!out2.contains("[LegalNotice]"));
    }

    #[test]
    fn respects_section_boundaries() {
        // keys must land inside [Preferences], not in a later section
        let ini = "[Preferences]\r\nGeneral\\Locale=en\r\n[RSS]\r\nFeeds=none\r\n";
        let out = ensure_qbt_ini(ini, None, false);
        let prefs_pos = out.find("[Preferences]").unwrap();
        let rss_pos = out.find("[RSS]").unwrap();
        let enabled_pos = out.find(r"WebUI\Enabled=true").unwrap();
        assert!(enabled_pos > prefs_pos && enabled_pos < rss_pos);
    }
}
