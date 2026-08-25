//! First-run setup: detect qBittorrent / Prowlarr, install and configure what's
//! missing. Prowlarr becomes a Trawler-managed instance (user-scope, no admin);
//! qBittorrent installs from its official release and gets its WebUI enabled by
//! editing its ini while it isn't running. Progress streams to the UI as
//! `setup-step` events.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use fs2::FileExt;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};

use crate::error::{AppError, Result};
use crate::AppState;

fn emit(app: &AppHandle, component: &str, kind: &str, payload: Value) {
    if let Some(msg) = payload.get("message").and_then(|m| m.as_str()) {
        if kind == "error" {
            crate::applog::error("setup", format!("{component}: {msg}"));
        } else {
            crate::applog::info("setup", format!("{component}: {msg}"));
        }
    }
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
    #[cfg(windows)]
    const EXE: &str = "Prowlarr.exe";
    #[cfg(not(windows))]
    const EXE: &str = "Prowlarr";
    let new = local_app_data().join("TrawlerTools").join("Prowlarr").join(EXE);
    if new.exists() {
        return new;
    }
    let legacy = local_app_data().join("Trawler").join("Prowlarr").join(EXE);
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

/// Match Prowlarr's own startup check: creating a file is not enough if the
/// process cannot also write and delete it. The unique name means a crashed
/// prior probe can never make a healthy folder look broken.
fn folder_write_probe(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    let mut nonce = [0u8; 8];
    if getrandom::getrandom(&mut nonce).is_err() {
        nonce.copy_from_slice(&(crate::db::now() as u64).to_le_bytes());
    }
    let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let probe = path.join(format!(".trawler-write-test-{suffix}.tmp"));
    let result = (|| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        file.write_all(b"Trawler managed Prowlarr write test")?;
        file.sync_all()?;
        drop(file);
        std::fs::remove_file(&probe)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&probe);
    }
    result
}

#[cfg(any(windows, test))]
fn parse_windows_sid(whoami_csv: &str) -> Option<String> {
    whoami_csv.lines().find_map(|line| {
        let sid = line.rsplit(',').next()?.trim().trim_matches('"');
        let body = sid.strip_prefix("S-")?;
        (body.starts_with("1-")
            && sid.len() <= 192
            && body.chars().all(|c| c.is_ascii_digit() || c == '-'))
        .then(|| sid.to_string())
    })
}

/// Prowlarr's Windows startup adds an inheritable Everyone/Modify ACL before
/// its write test. On Windows ARM the x64 build can fail during that ACL path.
/// Seed the same rule with native icacls plus an explicit rule for the current
/// user, so Prowlarr sees its intended ACL already present and leaves it alone.
#[cfg(windows)]
fn repair_windows_prowlarr_acl(path: &Path) -> std::result::Result<(), String> {
    let mut whoami = std::process::Command::new("whoami.exe");
    whoami.args(["/user", "/fo", "csv", "/nh"]);
    {
        use std::os::windows::process::CommandExt;
        whoami.creation_flags(0x0800_0000);
    }
    let whoami = whoami
        .output()
        .map_err(|e| format!("couldn't read the current Windows user SID: {e}"))?;
    if !whoami.status.success() {
        let detail = String::from_utf8_lossy(&whoami.stderr);
        return Err(format!(
            "Windows could not determine the current user SID: {}",
            detail.trim().chars().take(240).collect::<String>()
        ));
    }
    let sid = parse_windows_sid(&String::from_utf8_lossy(&whoami.stdout))
        .ok_or_else(|| "Windows did not return a usable current-user SID".to_string())?;

    let current_user_rule = format!("*{sid}:(OI)(CI)M");
    // S-1-1-0 is Everyone. IO makes this the same inherit-only rule Prowlarr
    // itself adds; access to the directory is supplied by the user rule.
    let prowlarr_rule = "*S-1-1-0:(OI)(CI)(IO)M";
    let mut icacls = std::process::Command::new("icacls.exe");
    icacls
        .arg(path)
        .args(["/inheritance:e", "/grant"])
        .arg(current_user_rule)
        .arg(prowlarr_rule)
        .arg("/Q");
    {
        use std::os::windows::process::CommandExt;
        icacls.creation_flags(0x0800_0000);
    }
    let output = icacls
        .output()
        .map_err(|e| format!("couldn't start Windows ACL repair: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "Windows ACL repair failed: {}",
            detail.trim().chars().take(240).collect::<String>()
        ))
    }
}

fn ensure_prowlarr_data_writable(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;

    #[cfg(not(windows))]
    return folder_write_probe(path).map_err(|error| {
        AppError::Other(format!(
            "Prowlarr's data folder is not writable ({}): {error}",
            path.display()
        ))
    });

    #[cfg(windows)]
    {
        repair_windows_prowlarr_acl(path).map_err(|error| {
            AppError::Other(format!(
                "Could not prepare Prowlarr's Windows data permissions ({}): {error}",
                path.display()
            ))
        })?;
        folder_write_probe(path).map_err(|error| {
            AppError::Other(format!(
                "Prowlarr's data folder is not writable after Windows permission repair ({}): {error}",
                path.display()
            ))
        })?;
        Ok(())
    }
}

#[cfg(windows)]
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

#[cfg(target_os = "macos")]
fn qbt_exe_candidates() -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("/Applications/qbittorrent.app/Contents/MacOS/qbittorrent")];
    if let Some(home) = dirs::home_dir() {
        out.push(home.join("Applications/qbittorrent.app/Contents/MacOS/qbittorrent"));
    }
    out
}

#[cfg(not(any(windows, target_os = "macos")))]
fn qbt_exe_candidates() -> Vec<PathBuf> {
    let mut out = vec![managed_qbt_appimage()];
    out.extend([
        PathBuf::from("/usr/bin/qbittorrent"),
        PathBuf::from("/usr/local/bin/qbittorrent"),
        PathBuf::from("/usr/bin/qbittorrent-nox"),
        PathBuf::from("/usr/local/bin/qbittorrent-nox"),
    ]);
    out
}

#[cfg(target_os = "linux")]
fn managed_qbt_appimage() -> PathBuf {
    local_app_data()
        .join("TrawlerTools")
        .join("qBittorrent")
        .join("qbittorrent.AppImage")
}

/// InstallLocation from qBittorrent's uninstall key (HKCU then HKLM).
#[cfg(windows)]
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

#[cfg(target_os = "macos")]
fn qbt_ini_path() -> PathBuf {
    // qBittorrent on macOS ignores Application Support for settings and reads
    // ~/.config/qBittorrent/qBittorrent.ini — verified against 5.2.3
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("qBittorrent")
        .join("qBittorrent.ini")
}

#[cfg(not(target_os = "macos"))]
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

#[cfg(windows)]
fn process_running(name: &str) -> bool {
    let mut cmd = std::process::Command::new("tasklist");
    cmd.args(["/FI", &format!("IMAGENAME eq {name}"), "/FO", "CSV", "/NH"]);
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(name))
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn process_running(name: &str) -> bool {
    // pgrep matches the BINARY name — strip any .exe a shared caller passes
    let name = name.trim_end_matches(".exe");
    std::process::Command::new("pgrep")
        .args(["-x", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn qbt_process_running() -> bool {
    if process_running("qbittorrent.exe") {
        return true;
    }
    #[cfg(target_os = "linux")]
    return process_running("qbittorrent-nox");
    #[allow(unreachable_code)]
    false
}

fn stop_qbt(force: bool) {
    if force {
        kill_process_force("qbittorrent.exe");
    } else {
        kill_process("qbittorrent.exe");
    }
    #[cfg(target_os = "linux")]
    if force {
        kill_process_force("qbittorrent-nox");
    } else {
        kill_process("qbittorrent-nox");
    }
}

/// Ask a process to stop, per-platform, POLITELY — the target gets to flush
/// its state (qBittorrent only writes qBittorrent.ini on a clean exit, so a
/// force-kill here silently reverts the user's preferences). Callers that
/// must win escalate with kill_process_force after a grace period.
pub(crate) fn kill_process(name: &str) {
    #[cfg(windows)]
    {
        // no /F: posts WM_CLOSE so the app can save, instead of terminating
        let mut kill = std::process::Command::new("taskkill");
        kill.args(["/IM", name]);
        use std::os::windows::process::CommandExt;
        kill.creation_flags(0x0800_0000);
        let _ = kill.output();
    }
    #[cfg(not(windows))]
    {
        let name = name.trim_end_matches(".exe");
        let _ = std::process::Command::new("pkill").args(["-x", name]).output();
    }
}

/// The no-really version: SIGKILL on unix, same taskkill /F on Windows.
pub(crate) fn kill_process_force(name: &str) {
    #[cfg(windows)]
    {
        let mut kill = std::process::Command::new("taskkill");
        kill.args(["/IM", name, "/F"]);
        use std::os::windows::process::CommandExt;
        kill.creation_flags(0x0800_0000);
        let _ = kill.output();
    }
    #[cfg(not(windows))]
    {
        let name = name.trim_end_matches(".exe");
        let _ = std::process::Command::new("pkill").args(["-9", "-x", name]).output();
    }
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
    } else if qbt_process_running() {
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

fn spawn_detached(exe: &PathBuf, args: &[String]) -> Result<u32> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    #[cfg(target_os = "linux")]
    if exe.extension().and_then(|ext| ext.to_str()) == Some("AppImage") {
        // Keep the user-scope qBittorrent route working where FUSE is absent.
        // The runtime extracts once for this long-lived process and cleans up
        // when qBittorrent exits.
        cmd.env("APPIMAGE_EXTRACT_AND_RUN", "1");
    }
    // children must not inherit our stdout/stderr — a chatty child writing
    // into a pipe nobody drains (Trawler launched by a launcher) blocks
    // forever, and a terminal launch gets log spam
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: survives Trawler exiting
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    #[cfg(unix)]
    {
        // own process group: survives Trawler exiting, immune to our signals
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::Other(format!("failed to start {}: {e}", exe.display())))?;
    let pid = child.id();
    // reap on a detached thread so an exited child never lingers as a zombie
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

/// 32 hex chars from OS entropy (RandomState seeds from the OS CSPRNG).
fn random_api_key() -> String {
    // real OS entropy — this feeds the Prowlarr API key and the qBittorrent
    // password + PBKDF2 salt, so a hasher-counter shortcut isn't acceptable
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS entropy unavailable");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
    ensure_prowlarr_data_writable(&data)?;
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
    // Prowlarr's self-updater would swap in fresh UNSIGNED binaries — on
    // Apple Silicon that bricks the install (silent SIGKILL). Trawler owns
    // this instance's lifecycle, so updates go through us on every platform.
    xml = upsert_xml_tag(&xml, "UpdateMechanism", "External");
    xml = upsert_xml_tag(&xml, "UpdateAutomatically", "False");
    let has_user_auth = xml.contains("<AuthenticationMethod>Forms</AuthenticationMethod>")
        || xml.contains("<AuthenticationMethod>Basic</AuthenticationMethod>");
    if !has_user_auth {
        xml = upsert_xml_tag(&xml, "AuthenticationMethod", "External");
        xml = upsert_xml_tag(&xml, "AuthenticationRequired", "DisabledForLocalAddresses");
    }
    std::fs::write(&path, xml)?;
    Ok(key)
}

/// Ad-hoc sign every executable in a managed Prowlarr tree. Failure on the
/// main binary is fatal — an unsigned binary is a guaranteed silent SIGKILL.
#[cfg(target_os = "macos")]
fn sign_prowlarr_tree(target: &std::path::Path) -> Result<()> {
    let main = std::process::Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(target.join("Prowlarr"))
        .output()
        .map_err(|e| AppError::Other(format!("codesign: {e}")))?;
    if !main.status.success() {
        return Err(AppError::Other(format!(
            "couldn't sign Prowlarr for macOS: {}",
            String::from_utf8_lossy(&main.stderr).chars().take(200).collect::<String>()
        )));
    }
    // every dylib AND every other executable (Prowlarr.Update ships its own
    // apphost) — an unsigned straggler bricks updates the same silent way
    if let Ok(walk) = std::process::Command::new("find")
        .arg(target)
        .args(["-type", "f", "(", "-name", "*.dylib", "-o", "-perm", "-111", ")"])
        .output()
    {
        for line in String::from_utf8_lossy(&walk.stdout).lines() {
            let _ = std::process::Command::new("codesign")
                .args(["--force", "--sign", "-", line])
                .output();
        }
    }
    Ok(())
}

struct ProwlarrLifecycleGuard<'a> {
    flag: &'a AtomicBool,
    lock_file: std::fs::File,
}

impl Drop for ProwlarrLifecycleGuard<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock_file);
        self.flag.store(false, Ordering::Release);
    }
}

fn prowlarr_lifecycle_lock_path() -> PathBuf {
    local_app_data().join("TrawlerTools").join(".prowlarr-lifecycle.lock")
}

fn claim_prowlarr_lifecycle_at<'a>(
    flag: &'a AtomicBool,
    lock_path: &Path,
) -> Result<ProwlarrLifecycleGuard<'a>> {
    if flag
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(AppError::Other("Prowlarr setup is already in progress".into()));
    }

    let result = (|| {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(lock_path)?;
        lock_file.try_lock_exclusive().map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                AppError::Other("Prowlarr setup is already in progress in another Trawler window".into())
            } else {
                AppError::Other(format!("couldn't lock Prowlarr's lifecycle: {error}"))
            }
        })?;
        Ok(ProwlarrLifecycleGuard { flag, lock_file })
    })();
    if result.is_err() {
        flag.store(false, Ordering::Release);
    }
    result
}

fn claim_prowlarr_lifecycle(flag: &AtomicBool) -> Result<ProwlarrLifecycleGuard<'_>> {
    claim_prowlarr_lifecycle_at(flag, &prowlarr_lifecycle_lock_path())
}

fn managed_prowlarr_pid_path() -> PathBuf {
    managed_prowlarr_data().join(".trawler-prowlarr.pid")
}

fn record_managed_prowlarr_pid(pid: u32) -> Result<()> {
    let path = managed_prowlarr_pid_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, pid.to_string())?;
    Ok(())
}

fn clear_managed_prowlarr_pid(pid: u32) {
    let path = managed_prowlarr_pid_path();
    let matches = std::fs::read_to_string(&path)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        == Some(pid);
    if matches {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(windows)]
fn process_command(pid: u32) -> Option<String> {
    let script = format!(
        "$p=Get-CimInstance Win32_Process -Filter \"ProcessId = {pid}\"; if ($null -ne $p) {{ [Console]::OutputEncoding=[Text.UTF8Encoding]::new(); [Console]::WriteLine($p.ExecutablePath); [Console]::WriteLine($p.CommandLine) }}"
    );
    let mut command = std::process::Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(not(windows))]
fn process_command(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn process_command_matches_managed(command: &str, exe: &Path, data: &Path) -> bool {
    #[cfg(windows)]
    let normalize = |value: &str| value.replace('\\', "/").to_ascii_lowercase();
    #[cfg(not(windows))]
    let normalize = |value: &str| value.to_string();

    let command = normalize(command);
    let exe = normalize(&exe.to_string_lossy());
    let data_arg = normalize(&format!("-data={}", data.display()));
    command.contains(&exe) && command.contains(&data_arg)
}

fn managed_prowlarr_pid() -> Option<u32> {
    let path = managed_prowlarr_pid_path();
    let pid = std::fs::read_to_string(&path).ok()?.trim().parse::<u32>().ok()?;
    let matches = process_command(pid).is_some_and(|command| {
        process_command_matches_managed(
            &command,
            &managed_prowlarr_exe(),
            &managed_prowlarr_data(),
        )
    });
    if matches {
        Some(pid)
    } else {
        clear_managed_prowlarr_pid(pid);
        None
    }
}

fn terminate_managed_prowlarr_pid(pid: u32, force: bool) -> bool {
    if managed_prowlarr_pid() != Some(pid) {
        return false;
    }
    #[cfg(windows)]
    let status = {
        let mut command = std::process::Command::new("taskkill");
        command.args(["/PID", &pid.to_string()]);
        if force {
            command.arg("/F");
        }
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
        command.status()
    };
    #[cfg(not(windows))]
    let status = std::process::Command::new("kill")
        .args([if force { "-KILL" } else { "-TERM" }, &pid.to_string()])
        .status();
    status.map(|status| status.success()).unwrap_or(false)
}

fn spawn_managed_prowlarr() -> Result<()> {
    let exe = managed_prowlarr_exe();
    if !exe.exists() {
        return Err(AppError::Other("no managed Prowlarr install found".into()));
    }
    // Prowlarr's self-updater replaces binaries with fresh unsigned ones —
    // re-signing here is idempotent and cheap, and un-bricks that case
    #[cfg(target_os = "macos")]
    if let Some(dir) = exe.parent() {
        let _ = sign_prowlarr_tree(dir);
    }
    seed_managed_prowlarr_config()?;
    let data = managed_prowlarr_data();
    let pid = spawn_detached(&exe, &[format!("-data={}", data.display())])?;
    if let Err(error) = record_managed_prowlarr_pid(pid) {
        crate::applog::error(
            "setup",
            format!("Prowlarr started as PID {pid}, but its ownership record could not be saved: {error}"),
        );
    }
    Ok(())
}

/// Does the process answering on :9696 authenticate with OUR seeded API
/// key? The unauthenticated ping proves only that SOMETHING is there.
async fn prowlarr_is_ours(state: &AppState, key: &str) -> bool {
    state
        .http
        .get("http://127.0.0.1:9696/api/v1/system/status")
        .header("X-Api-Key", key)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

async fn wait_for_prowlarr(state: &AppState, secs: u64) -> bool {
    // Only the seeded API key proves OUR instance came up — a foreign
    // Prowlarr the user runs themselves answers the ping too, while our
    // freshly spawned instance dies unable to bind the port.
    let key = match read_prowlarr_api_key() {
        Some(k) => k,
        None => return false,
    };
    for _ in 0..secs {
        if prowlarr_is_ours(state, &key).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    false
}

async fn start_managed_prowlarr_inner(state: &AppState) -> Result<()> {
    // Give an already-starting managed instance a short chance to identify
    // itself before launching anything. This covers the first upgrade from a
    // version that did not yet persist its managed PID.
    if wait_for_prowlarr(state, 5).await {
        return Ok(());
    }

    // A PID is trusted only when its command line contains both our managed
    // executable and our managed data directory. Foreign Prowlarr processes
    // neither block startup nor become termination targets.
    if let Some(pid) = managed_prowlarr_pid() {
        for _ in 0..40 {
            if wait_for_prowlarr(state, 1).await {
                return Ok(());
            }
            if managed_prowlarr_pid() != Some(pid) {
                break;
            }
        }
        if managed_prowlarr_pid() == Some(pid) {
            return Err(AppError::Other(format!(
                "the managed Prowlarr process (PID {pid}) is running but did not become ready on :9696"
            )));
        }
    }

    spawn_managed_prowlarr()?;
    if wait_for_prowlarr(state, 45).await {
        Ok(())
    } else {
        Err(AppError::Other(
            "Prowlarr started but didn't come up on :9696 within 45 seconds".into(),
        ))
    }
}

async fn stop_managed_prowlarr(state: &AppState, key: &str) -> Result<()> {
    let response = state
        .http
        .post("http://127.0.0.1:9696/api/v1/system/shutdown")
        .header("X-Api-Key", key)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map_err(|error| AppError::Other(format!("couldn't ask managed Prowlarr to stop: {error}")))?;
    if !response.status().is_success() {
        return Err(AppError::Other(format!(
            "managed Prowlarr rejected its authenticated shutdown request ({})",
            response.status()
        )));
    }

    for _ in 0..30 {
        if !prowlarr_is_ours(state, key).await && managed_prowlarr_pid().is_none() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // The API shutdown is the normal path. Escalation is allowed only for the
    // recorded PID after its executable and -data argument are revalidated.
    if let Some(pid) = managed_prowlarr_pid() {
        if terminate_managed_prowlarr_pid(pid, true) {
            for _ in 0..20 {
                if !prowlarr_is_ours(state, key).await && managed_prowlarr_pid().is_none() {
                    clear_managed_prowlarr_pid(pid);
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
    Err(AppError::Other(
        "managed Prowlarr did not stop; its existing files were left untouched".into(),
    ))
}

pub async fn start_managed_prowlarr(state: &AppState) -> Result<()> {
    let _lifecycle = claim_prowlarr_lifecycle(&state.prowlarr_busy)?;
    start_managed_prowlarr_inner(state).await
}

fn read_prowlarr_api_key() -> Option<String> {
    let xml = std::fs::read_to_string(managed_prowlarr_data().join("config.xml")).ok()?;
    let start = xml.find("<ApiKey>")? + "<ApiKey>".len();
    let end = xml[start..].find("</ApiKey>")? + start;
    let key = xml[start..end].trim().to_string();
    if key.is_empty() { None } else { Some(key) }
}

#[derive(Debug)]
struct ProwlarrAsset {
    url: String,
    sha256: String,
}

fn prowlarr_asset(release: &Value, suffix: &str) -> Result<ProwlarrAsset> {
    let asset = release
        .get("assets")
        .and_then(Value::as_array)
        .and_then(|assets| {
            assets.iter().find(|asset| {
                asset
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.ends_with(suffix))
            })
        })
        .ok_or_else(|| AppError::Other("could not find a Prowlarr build for this platform".into()))?;
    let url = asset
        .get("browser_download_url")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Other("Prowlarr's release asset had no download URL".into()))?;
    if !(url.starts_with("https://github.com/Prowlarr/Prowlarr/")
        || url.starts_with("https://objects.githubusercontent.com/"))
    {
        return Err(AppError::Other(
            "Prowlarr's release URL looked wrong — not downloading it".into(),
        ));
    }
    let sha256 = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| AppError::Other("Prowlarr's release asset had no valid SHA-256 digest".into()))?;
    Ok(ProwlarrAsset {
        url: url.into(),
        sha256: sha256.to_ascii_lowercase(),
    })
}

struct StagingDirectory(Option<PathBuf>);

impl StagingDirectory {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn unique_sibling_path(target: &Path, purpose: &str) -> Result<PathBuf> {
    let parent = target
        .parent()
        .ok_or_else(|| AppError::Other("Prowlarr's install path had no parent directory".into()))?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Prowlarr");
    for _ in 0..32 {
        let candidate = parent.join(format!(".{name}.{purpose}-{}", random_api_key()));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(AppError::Other(format!(
        "couldn't reserve a unique Prowlarr {purpose} path"
    )))
}

fn create_unique_staging_dir(target: &Path) -> Result<PathBuf> {
    let parent = target
        .parent()
        .ok_or_else(|| AppError::Other("Prowlarr's install path had no parent directory".into()))?;
    std::fs::create_dir_all(parent)?;
    for _ in 0..32 {
        let candidate = unique_sibling_path(target, "stage")?;
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(AppError::Other(
        "couldn't create a unique Prowlarr staging directory".into(),
    ))
}

#[derive(Debug)]
struct PendingDirectoryActivation {
    backup: Option<PathBuf>,
}

impl PendingDirectoryActivation {
    fn backup_path(&self) -> Option<&Path> {
        self.backup.as_deref()
    }

    fn commit(mut self) {
        if let Some(backup) = self.backup.take() {
            if let Err(error) = std::fs::remove_dir_all(&backup) {
                crate::applog::error(
                    "setup",
                    format!(
                        "Prowlarr updated, but its old backup at {} could not be removed: {error}",
                        backup.display()
                    ),
                );
            }
        }
    }
}

fn replace_directory_rollback_safe(
    staged: &Path,
    final_target: &Path,
) -> Result<PendingDirectoryActivation> {
    if !staged.is_dir() {
        return Err(AppError::Other(format!(
            "Prowlarr's staged install is missing: {}",
            staged.display()
        )));
    }
    if !final_target.exists() {
        std::fs::rename(staged, final_target).map_err(|error| {
            AppError::Other(format!("couldn't activate the staged Prowlarr install: {error}"))
        })?;
        return Ok(PendingDirectoryActivation { backup: None });
    }

    let backup = unique_sibling_path(final_target, "backup")?;
    std::fs::rename(final_target, &backup).map_err(|error| {
        AppError::Other(format!(
            "couldn't move the existing Prowlarr aside; its files may still be in use ({error})"
        ))
    })?;
    if let Err(activate_error) = std::fs::rename(staged, final_target) {
        return match std::fs::rename(&backup, final_target) {
            Ok(()) => Err(AppError::Other(format!(
                "couldn't activate the staged Prowlarr install; the previous install was restored ({activate_error})"
            ))),
            Err(rollback_error) => Err(AppError::Other(format!(
                "couldn't activate the staged Prowlarr install ({activate_error}), and couldn't restore the previous install from {} ({rollback_error})",
                backup.display()
            ))),
        };
    }

    Ok(PendingDirectoryActivation {
        backup: Some(backup),
    })
}

#[cfg(any(windows, test))]
fn verify_extracted_files(target: &Path, manifest: &[(PathBuf, u64)]) -> Result<()> {
    if manifest.is_empty() {
        return Err(AppError::Other("the Prowlarr archive contained no files".into()));
    }
    for (relative, expected_size) in manifest {
        let path = target.join(relative);
        let metadata = std::fs::metadata(&path).map_err(|error| {
            AppError::Other(format!(
                "Prowlarr extraction was incomplete: {} is missing ({error})",
                relative.display()
            ))
        })?;
        if !metadata.is_file() || metadata.len() != *expected_size {
            return Err(AppError::Other(format!(
                "Prowlarr extraction was incomplete: {} should be {expected_size} bytes but is {}",
                relative.display(),
                metadata.len()
            )));
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn extract_windows_prowlarr(bytes: Vec<u8>, target: &Path) -> Result<()> {
    use std::collections::HashSet;
    use std::ffi::OsStr;
    use std::path::Component;

    let cursor = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| AppError::Other(format!("bad archive: {e}")))?;
    let mut manifest = Vec::new();
    let mut seen = HashSet::new();
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| AppError::Other(format!("archive read: {e}")))?;
        if file.is_symlink() {
            return Err(AppError::Other(format!(
                "Prowlarr's archive contained a symbolic link: {}",
                file.name()
            )));
        }
        let path = file.enclosed_name().ok_or_else(|| {
            AppError::Other(format!("Prowlarr's archive contained an unsafe path: {}", file.name()))
        })?;
        let mut components = path.components();
        if !matches!(components.next(), Some(Component::Normal(root)) if root == OsStr::new("Prowlarr"))
        {
            return Err(AppError::Other(format!(
                "Prowlarr's archive had an unexpected top-level path: {}",
                path.display()
            )));
        }
        let stripped: PathBuf = components.collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let out = target.join(&stripped);
        if file.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if !seen.insert(stripped.clone()) {
            return Err(AppError::Other(format!(
                "Prowlarr's archive contained a duplicate file: {}",
                stripped.display()
            )));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let expected_size = file.size();
        let mut writer = std::fs::File::create(&out)?;
        let written = std::io::copy(&mut file, &mut writer)?;
        if written != expected_size {
            return Err(AppError::Other(format!(
                "Prowlarr extraction ended early for {} ({written} of {expected_size} bytes)",
                stripped.display()
            )));
        }
        manifest.push((stripped, expected_size));
    }
    verify_extracted_files(target, &manifest)
}

fn verify_staged_prowlarr(target: &Path) -> Result<()> {
    #[cfg(windows)]
    const EXE: &str = "Prowlarr.exe";
    #[cfg(not(windows))]
    const EXE: &str = "Prowlarr";
    let exe = target.join(EXE);
    if !exe.is_file() {
        return Err(AppError::Other(format!(
            "Prowlarr extraction completed without its main executable ({})",
            exe.display()
        )));
    }
    Ok(())
}

/// Download the latest Prowlarr, extract it user-scope, start it, capture the
/// API key, and write it into Trawler's config. No admin rights involved.
pub async fn install_prowlarr(app: &AppHandle) -> Result<String> {
    let state_guard = app.state::<AppState>();
    let state: &AppState = state_guard.inner();
    let _lifecycle = claim_prowlarr_lifecycle(&state.prowlarr_busy)?;

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
    // per-platform asset: windows zip, mac tar.gz matched to the CPU
    #[cfg(windows)]
    const ASSET_SUFFIX: &str = "windows-core-x64.zip";
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    const ASSET_SUFFIX: &str = "osx-core-arm64.tar.gz";
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    const ASSET_SUFFIX: &str = "osx-core-x64.tar.gz";
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    const ASSET_SUFFIX: &str = "linux-core-x64.tar.gz";
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    const ASSET_SUFFIX: &str = "linux-core-arm64.tar.gz";
    #[cfg(all(target_os = "linux", target_arch = "arm"))]
    const ASSET_SUFFIX: &str = "linux-core-arm.tar.gz";
    let asset = prowlarr_asset(&release, ASSET_SUFFIX)?;

    emit(app, "prowlarr", "log", json!({ "message": "Downloading Prowlarr…" }));
    // ~100 MB: the client's 60s TOTAL deadline would abort this on any
    // connection under ~15 Mbps, mid-wizard, with a confusing error
    let resp = state
        .http
        .get(&asset.url)
        .timeout(std::time::Duration::from_secs(1800))
        .send()
        .await?
        .error_for_status()
        .map_err(|e| AppError::Other(format!("Prowlarr download failed: {e}")))?;
    let total = resp.content_length().unwrap_or(0);
    let mut bytes: Vec<u8> = Vec::with_capacity(total as usize);
    let mut hasher = Sha256::new();
    let mut stream = resp;
    let mut last_pct = 0u32;
    while let Some(chunk) = stream.chunk().await? {
        hasher.update(&chunk);
        bytes.extend_from_slice(&chunk);
        if total > 0 {
            let pct = (bytes.len() as u64 * 100 / total) as u32;
            if pct >= last_pct + 5 {
                last_pct = pct;
                emit(app, "prowlarr", "progress", json!({ "pct": pct }));
            }
        }
    }
    if total > 0 && bytes.len() as u64 != total {
        return Err(AppError::Other(format!(
            "the Prowlarr download ended early ({} of {total} bytes)",
            bytes.len()
        )));
    }
    if format!("{:x}", hasher.finalize()) != asset.sha256 {
        return Err(AppError::Other(
            "the downloaded Prowlarr archive failed its SHA-256 check — refusing to install it"
                .into(),
        ));
    }

    emit(app, "prowlarr", "log", json!({ "message": "Extracting…" }));
    // stage + swap: a failed extract or signing pass must never leave a
    // half-install that looks installed to status probes and boot hooks
    let final_target = managed_prowlarr_dir();
    let target = create_unique_staging_dir(&final_target)?;
    let mut staging_guard = StagingDirectory(Some(target.clone()));
    #[cfg(windows)]
    {
        extract_windows_prowlarr(bytes, &target)?;
    }
    #[cfg(not(windows))]
    {
        // tar.gz: hand it to the system tar, which preserves exec bits
        let tarball = target.join("prowlarr-download.tar.gz");
        std::fs::write(&tarball, &bytes)?;
        let ok = std::process::Command::new("tar")
            .args(["-xzf"])
            .arg(&tarball)
            .args(["--strip-components=1", "-C"])
            .arg(&target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let _ = std::fs::remove_file(&tarball);
        if !ok {
            return Err(AppError::Other("couldn't extract the Prowlarr archive".into()));
        }
        // Apple Silicon SIGKILLs unsigned arm64 code with no output at all,
        // and Prowlarr's osx tarball ships unsigned — ad-hoc sign everything
        // executable so the kernel lets it run (verified: exit 137 without)
        #[cfg(target_os = "macos")]
        {
            emit(app, "prowlarr", "log", json!({ "message": "Signing for macOS…" }));
            sign_prowlarr_tree(&target)?;
        }
    }
    verify_staged_prowlarr(&target)?;

    // extraction + signing succeeded — stop any running instance of OURS
    // before swapping its files (Windows locks a running exe; unix keeps the
    // old process serving from unlinked inodes while the new one can't
    // bind). The seeded key proves the process on :9696 is ours — a foreign
    // Prowlarr the user runs themselves is never killed.
    if managed_prowlarr_exe().exists() {
        if let Some(key) = read_prowlarr_api_key() {
            if prowlarr_is_ours(state, &key).await {
                emit(app, "prowlarr", "log", json!({ "message": "Stopping the running Prowlarr…" }));
                stop_managed_prowlarr(state, &key).await?;
            }
        }
    }
    if let Some(pid) = managed_prowlarr_pid() {
        return Err(AppError::Other(format!(
            "managed Prowlarr is still starting as PID {pid}; its existing files were left untouched — retry when it is ready"
        )));
    }

    // Activate only after the complete staged tree has been verified. The old
    // install is moved aside, never recursively deleted in place; activation
    // failure restores it.
    let activation = replace_directory_rollback_safe(&target, &final_target)?;
    staging_guard.disarm();

    let setup_result: Result<String> = async {
        // Seed config BEFORE first boot: our own API key, localhost-only, no
        // browser popup, no admin-login form. First impressions matter.
        emit(app, "prowlarr", "log", json!({ "message": "Configuring…" }));
        let key = seed_managed_prowlarr_config()?;

        emit(app, "prowlarr", "log", json!({ "message": "Starting Prowlarr…" }));
        start_managed_prowlarr_inner(state).await?;

        {
            let mut cfg = state.config.write().await;
            cfg.prowlarr_url = "http://127.0.0.1:9696".into();
            cfg.prowlarr_api_key = key.clone();
            crate::config::save(&cfg)?;
        }
        Ok(key)
    }
    .await;
    let key = match setup_result {
        Ok(key) => {
            activation.commit();
            key
        }
        Err(error) => {
            if let Some(backup) = activation.backup_path() {
                return Err(AppError::Other(format!(
                    "{error}; the previous Prowlarr executables were retained at {}",
                    backup.display()
                )));
            }
            return Err(error);
        }
    };
    emit(app, "prowlarr", "done", json!({ "message": "Prowlarr is running and connected" }));
    Ok(key)
}

/// Windows silently reserves random port ranges for Hyper-V/WSL/Docker NAT;
/// binding inside one fails with WSAEACCES and no visible symptom beyond a
/// permanently dead session (verified live: qBt's random port landed in a
/// reserved block and every torrent showed 0 seeds forever).
#[cfg(windows)]
pub fn excluded_port_ranges() -> Vec<(u16, u16)> {
    let mut out = vec![];
    let mut any_ok = false;
    for proto in ["udp", "tcp"] {
        let mut cmd = std::process::Command::new("netsh");
        cmd.args(["interface", "ipv4", "show", "excludedportrange", &format!("protocol={proto}")]);
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        if let Ok(o) = cmd.output() {
            if o.status.success() {
                any_ok = true;
            }
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let nums: Vec<u16> = line
                    .split_whitespace()
                    .filter_map(|t| t.parse::<u16>().ok())
                    .collect();
                if nums.len() >= 2 {
                    out.push((nums[0], nums[1]));
                }
            }
        }
    }
    // this function exists to PREVENT fail-open — an empty answer from a
    // working netsh is plausible, but a failed netsh must be loud
    if !any_ok {
        crate::applog::warn("setup", "couldn't read Windows' reserved port ranges — falling back to bind-probing only");
    }
    out
}

#[cfg(not(windows))]
pub fn excluded_port_ranges() -> Vec<(u16, u16)> {
    vec![]
}

/// A listen port that is actually bindable: outside every reserved range AND
/// proven by binding both TCP and UDP on it — the direct test catches the
/// WSAEACCES reservations netsh describes plus plain port-in-use, on every
/// platform. Blocking (netsh + socket binds): call via spawn_blocking.
pub fn pick_safe_listen_port() -> u16 {
    let excluded = excluded_port_ranges();
    let outside_reserved = |p: u16| !excluded.iter().any(|(a, b)| p >= *a && p <= *b);
    let bindable = |p: u16| {
        std::net::TcpListener::bind(("0.0.0.0", p)).is_ok()
            && std::net::UdpSocket::bind(("0.0.0.0", p)).is_ok()
    };
    let mut seed = [0u8; 2];
    if getrandom::getrandom(&mut seed).is_err() {
        // never let all machines collapse onto one port
        let t = crate::db::now() as u16;
        seed = t.to_le_bytes();
    }
    let base = 20000 + (u16::from_le_bytes(seed) % 20000);
    for offset in 0..2000u16 {
        let p = 20000 + ((base - 20000 + offset * 7) % 20000);
        if outside_reserved(p) && bindable(p) {
            return p;
        }
    }
    28645 // the port that saved the day once already
}

/// The tail of qBittorrent's own log file — bind failures and session errors
/// live there and nowhere else.
pub fn qbt_log_tail(lines: usize) -> Option<String> {
    #[cfg(windows)]
    let path = dirs::data_local_dir()?.join("qBittorrent").join("logs").join("qbittorrent.log");
    #[cfg(target_os = "macos")]
    let path = {
        // settings live in ~/.config (verified against 5.2.3) — probe both
        // plausible log homes rather than asserting one
        let h = dirs::home_dir()?;
        let a = h.join(".config/qBittorrent/logs/qbittorrent.log");
        let b = h.join("Library/Application Support/qBittorrent/logs/qbittorrent.log");
        if a.exists() { a } else { b }
    };
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let path = dirs::data_local_dir()?.join("qBittorrent").join("logs").join("qbittorrent.log");
    // qBt's log grows to megabytes — read only the final chunk
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(&path).ok()?;
    let len = f.metadata().ok()?.len();
    let start_at = len.saturating_sub(64 * 1024);
    let _ = f.seek(SeekFrom::Start(start_at));
    let mut buf = String::new();
    let _ = f.read_to_string(&mut buf);
    let all: Vec<&str> = buf.lines().collect();
    let start = all.len().saturating_sub(lines);
    Some(all[start..].join("\n"))
}

// ---------------- flaresolverr (optional) ----------------
// FlareSolverr is strictly opt-in: the stack runs fine without it, it only
// unlocks Cloudflare-protected indexers (1337x, EZTV, …) for users who ask.

pub fn managed_flaresolverr_exe() -> PathBuf {
    #[cfg(windows)]
    const EXE: &str = "flaresolverr.exe";
    #[cfg(not(windows))]
    const EXE: &str = "flaresolverr";
    local_app_data().join("TrawlerTools").join("FlareSolverr").join(EXE)
}

fn flaresolverr_process_name() -> &'static str {
    #[cfg(windows)]
    return "flaresolverr.exe";
    #[cfg(not(windows))]
    return "flaresolverr";
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
    #[cfg(target_os = "macos")]
    {
        let docker = docker_path()
            .ok_or_else(|| AppError::Other("Docker's CLI can't be found — is Docker Desktop running?".into()))?;
        let out = std::process::Command::new(docker)
            .args(["start", FS_CONTAINER])
            .output()
            .map_err(|e| AppError::Other(format!("docker: {e}")))?;
        if !out.status.success() {
            return Err(AppError::Other("the FlareSolverr container wouldn't start".into()));
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        let exe = managed_flaresolverr_exe();
        if !exe.exists() {
            return Err(AppError::Other("FlareSolverr isn't installed".into()));
        }
        spawn_detached(&exe, &[]).map(|_| ())
    }
}

/// Tear the managed FlareSolverr down locally (process/container + files).
pub fn remove_flaresolverr_local() -> Result<()> {
    let _ = std::fs::remove_file(flaresolverr_marker());
    #[cfg(target_os = "macos")]
    {
        let docker = docker_path()
            .ok_or_else(|| AppError::Other("Docker's CLI can't be found, so the container can't be removed — remove trawler-flaresolverr from Docker Desktop yourself".into()))?;
        let out = std::process::Command::new(docker)
            .args(["rm", "-f", FS_CONTAINER])
            .output()
            .map_err(|e| AppError::Other(format!("docker: {e}")))?;
        let err = String::from_utf8_lossy(&out.stderr);
        // an already-gone container is a success, anything else is not
        if !out.status.success() && !err.contains("No such container") {
            return Err(AppError::Other(format!(
                "couldn't remove the FlareSolverr container: {}",
                err.chars().take(200).collect::<String>()
            )));
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        kill_process(flaresolverr_process_name());
        std::thread::sleep(std::time::Duration::from_millis(800));
        if process_running(flaresolverr_process_name()) {
            // windowless process — the polite close rarely lands; escalate
            kill_process_force(flaresolverr_process_name());
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
        if let Some(dir) = managed_flaresolverr_exe().parent().map(PathBuf::from) {
            if dir.exists() {
                std::fs::remove_dir_all(&dir)
                    .map_err(|e| AppError::Other(format!("couldn't remove {}: {e}", dir.display())))?;
            }
        }
        Ok(())
    }
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

/// Docker's CLI is NOT on a Finder-launched app's PATH (launchd gives us
/// /usr/bin:/bin:/usr/sbin:/sbin) — resolve the real binary or the whole
/// route only works from a terminal.
#[cfg(target_os = "macos")]
fn docker_path() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/usr/local/bin/docker"),
        PathBuf::from("/opt/homebrew/bin/docker"),
        PathBuf::from("/Applications/Docker.app/Contents/Resources/bin/docker"),
    ];
    if let Some(home) = dirs::home_dir() {
        candidates.insert(0, home.join(".docker/bin/docker"));
    }
    candidates.into_iter().find(|p| p.exists())
}

/// Is Docker usable? (macOS path: FlareSolverr ships no darwin binary.)
#[cfg(target_os = "macos")]
fn docker_available() -> bool {
    let Some(docker) = docker_path() else { return false };
    std::process::Command::new(docker)
        .args(["info", "--format", "ok"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The opt-in marker: a stat is cheap enough for boot hooks and status polls,
/// where a docker subprocess per check is not.
fn flaresolverr_marker() -> PathBuf {
    local_app_data().join("TrawlerTools").join(".flaresolverr-optin")
}

/// The container name the managed Docker route owns.
#[cfg(target_os = "macos")]
const FS_CONTAINER: &str = "trawler-flaresolverr";

/// macOS: FlareSolverr publishes no darwin binary, so the managed route runs
/// the official container instead. Still strictly opt-in.
#[cfg(target_os = "macos")]
pub async fn install_flaresolverr(app: &AppHandle) -> Result<()> {
    let state_guard = app.state::<AppState>();
    let state: &AppState = state_guard.inner();
    if !docker_available() {
        return Err(AppError::Other(
            "FlareSolverr has no Mac build, so Trawler runs it via Docker — install Docker Desktop (docker.com) first, then click again".into(),
        ));
    }
    let docker = docker_path().ok_or_else(|| {
        AppError::Other("Docker looks installed but its CLI can't be found — is Docker Desktop running?".into())
    })?;
    // pin the image: :latest is silent supply-chain drift, and the pull is
    // ~1 GB — do it as its own step so the UI can say why it's slow
    const FS_IMAGE: &str = "ghcr.io/flaresolverr/flaresolverr:v3.5.0";
    emit(app, "flaresolverr", "log", json!({ "message": "Pulling the FlareSolverr image (~1 GB — first time can take several minutes)…" }));
    let pull_docker = docker.clone();
    let pull = tokio::task::spawn_blocking(move || {
        std::process::Command::new(pull_docker).args(["pull", FS_IMAGE]).output()
    })
    .await
    .map_err(|e| AppError::Other(format!("docker pull task: {e}")))?
    .map_err(|e| AppError::Other(format!("docker: {e}")))?;
    if !pull.status.success() {
        return Err(AppError::Other(format!(
            "couldn't pull the FlareSolverr image: {}",
            String::from_utf8_lossy(&pull.stderr).chars().take(200).collect::<String>()
        )));
    }
    emit(app, "flaresolverr", "log", json!({ "message": "Starting the FlareSolverr container…" }));
    // a previous container (stopped or stale) gets replaced, not duplicated;
    // loopback-only publish — Docker Desktop bypasses the macOS firewall, and
    // an open headless-browser proxy must not be reachable from the LAN
    let run_docker = docker.clone();
    let run = tokio::task::spawn_blocking(move || {
        let _ = std::process::Command::new(&run_docker).args(["rm", "-f", FS_CONTAINER]).output();
        std::process::Command::new(&run_docker)
            .args(["run", "-d", "--name", FS_CONTAINER, "-p", "127.0.0.1:8191:8191", FS_IMAGE])
            .output()
    })
    .await
    .map_err(|e| AppError::Other(format!("docker run task: {e}")))?
    .map_err(|e| AppError::Other(format!("docker: {e}")))?;
    if !run.status.success() {
        return Err(AppError::Other(format!(
            "the FlareSolverr container refused to start: {}",
            String::from_utf8_lossy(&run.stderr).chars().take(200).collect::<String>()
        )));
    }
    if !wait_for_flaresolverr(state, 90).await {
        return Err(AppError::Other("FlareSolverr's container started but :8191 never answered".into()));
    }
    let _ = std::fs::write(flaresolverr_marker(), b"");
    Ok(())
}

/// Does the managed FlareSolverr exist on this machine? (exe on Windows,
/// container on macOS)
pub fn flaresolverr_installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        return flaresolverr_marker().exists();
    }
    #[allow(unreachable_code)]
    managed_flaresolverr_exe().exists()
}

/// Download the latest FlareSolverr (it bundles a browser), extract it next to
/// managed Prowlarr, and start it. Opt-in only; never part of the wizard.
#[cfg(not(target_os = "macos"))]
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
    #[cfg(windows)]
    const ASSET_NAME: &str = "flaresolverr_windows_x64.zip";
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    const ASSET_NAME: &str = "flaresolverr_linux_x64.tar.gz";
    #[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
    const ASSET_NAME: &str = "";
    #[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
    {
        return Err(AppError::Other(
            "FlareSolverr does not publish an official Linux ARM build yet".into(),
        ));
    }
    let asset = release
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|assets| assets.iter().find(|asset| {
            asset.get("name").and_then(Value::as_str) == Some(ASSET_NAME)
        }))
        .ok_or_else(|| AppError::Other("could not find a FlareSolverr build for this platform".into()))?;
    let asset_url = asset
        .get("browser_download_url")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Other("FlareSolverr's release asset had no download URL".into()))?
        .to_string();
    let expected_sha256 = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| AppError::Other("FlareSolverr's release asset had no valid SHA-256 digest".into()))?
        .to_ascii_lowercase();
    // the URL came out of an API response — only fetch a real GitHub asset host
    if !(asset_url.starts_with("https://github.com/FlareSolverr/")
        || asset_url.starts_with("https://objects.githubusercontent.com/"))
    {
        return Err(AppError::Other("FlareSolverr release URL looked wrong — not downloading it".into()));
    }

    emit(app, "flaresolverr", "log", json!({ "message": "Downloading FlareSolverr (it bundles a browser)…" }));
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
    #[cfg(windows)]
    let archive_path = tools.join("flaresolverr-download.zip");
    #[cfg(target_os = "linux")]
    let archive_path = tools.join("flaresolverr-download.tar.gz");
    // the large temporary archive must not survive any failure path
    struct TempFile(PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _archive_guard = TempFile(archive_path.clone());
    let mut hasher = Sha256::new();
    let downloaded = {
        let mut out = std::fs::File::create(&archive_path)?;
        let mut stream = resp;
        let mut got: u64 = 0;
        let mut last_pct = 0u32;
        while let Some(chunk) = stream.chunk().await? {
            use std::io::Write;
            out.write_all(&chunk)?;
            hasher.update(&chunk);
            got += chunk.len() as u64;
            if total > 0 {
                let pct = (got * 100 / total) as u32;
                if pct >= last_pct + 5 {
                    last_pct = pct;
                    emit(app, "flaresolverr", "progress", json!({ "pct": pct }));
                }
            }
        }
        got
    };
    if total > 0 && downloaded != total {
        return Err(AppError::Other(format!(
            "the download ended early ({downloaded} of {total} bytes) — check the connection and try again"
        )));
    }
    if format!("{:x}", hasher.finalize()) != expected_sha256 {
        return Err(AppError::Other(
            "the downloaded FlareSolverr archive failed its SHA-256 check — refusing to run it".into(),
        ));
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
    #[cfg(windows)]
    {
        let f = std::fs::File::open(&archive_path)?;
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
    #[cfg(target_os = "linux")]
    {
        let output = std::process::Command::new("tar")
            .args(["-xzf"])
            .arg(&archive_path)
            .args(["--strip-components=1", "-C"])
            .arg(&staging)
            .output()
            .map_err(|e| AppError::Other(format!("couldn't start tar: {e}")))?;
        if !output.status.success() {
            return Err(AppError::Other(format!(
                "couldn't extract FlareSolverr: {}",
                String::from_utf8_lossy(&output.stderr).chars().take(200).collect::<String>()
            )));
        }
    }
    let staged_exe = staging.join(managed_flaresolverr_exe().file_name().unwrap_or_default());
    if !staged_exe.is_file() {
        return Err(AppError::Other(
            "the FlareSolverr archive did not contain its expected executable".into(),
        ));
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
    if qbt_process_running() {
        stop_qbt(false);
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            if !qbt_process_running() {
                break;
            }
        }
        if qbt_process_running() {
            // saving resume data can outlast the polite window — escalate once
            stop_qbt(true);
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }
        if qbt_process_running() {
            #[cfg(target_os = "macos")]
            return Err(AppError::Other("couldn't close qBittorrent — quit it from the Dock and try again".into()));
            #[allow(unreachable_code)]
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

    #[cfg(target_os = "macos")]
    {
        // open the exact bundle we resolved — a by-name lookup can miss a
        // just-copied app or pick the wrong one of two copies; and check the
        // exit code, because /usr/bin/open itself always spawns fine
        let bundle = exe
            .ancestors()
            .nth(3)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Applications/qbittorrent.app"));
        let out = std::process::Command::new("open")
            .arg(&bundle)
            .output()
            .map_err(|e| AppError::Other(format!("couldn't launch qBittorrent: {e}")))?;
        if !out.status.success() {
            return Err(AppError::Other(format!(
                "couldn't launch qBittorrent: {}",
                String::from_utf8_lossy(&out.stderr).chars().take(200).collect::<String>()
            )));
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    spawn_detached(&exe, &[]).map(|_| ())
}

/// Install qBittorrent on macOS: official dmg from SourceForge, mounted and
/// copied to /Applications — no admin prompt, no package manager needed.
#[cfg(target_os = "macos")]
pub async fn install_qbt_macos(app: &AppHandle) -> Result<()> {
    let state_guard = app.state::<AppState>();
    let state: &AppState = state_guard.inner();
    if qbt_exe_candidates().iter().any(|p| p.exists()) {
        return Ok(());
    }
    emit(app, "qbit", "log", json!({ "message": "Finding the latest qBittorrent…" }));
    let rss = state
        .http
        .get("https://sourceforge.net/projects/qbittorrent/rss?path=/qbittorrent-mac")
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?
        .text()
        .await?;
    let ver = rss
        .split("qbittorrent-mac/qbittorrent-")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .ok_or_else(|| AppError::Other("couldn't find the latest qBittorrent for macOS".into()))?
        .to_string();
    // scraped text goes into a URL — accept nothing but a dotted version
    if ver.is_empty() || !ver.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(AppError::Other("qBittorrent's version listing looked wrong — not downloading".into()));
    }
    let url = format!(
        "https://downloads.sourceforge.net/project/qbittorrent/qbittorrent-mac/qbittorrent-{ver}/qbittorrent-{ver}.dmg"
    );
    emit(app, "qbit", "log", json!({ "message": format!("Downloading qBittorrent {ver}…") }));
    let resp = state
        .http
        .get(&url)
        .timeout(std::time::Duration::from_secs(1800))
        .send()
        .await?
        .error_for_status()
        .map_err(|e| AppError::Other(format!("qBittorrent download failed: {e}")))?;
    let tools = local_app_data().join("TrawlerTools");
    std::fs::create_dir_all(&tools)?;
    let dmg = tools.join("qbittorrent-download.dmg");
    struct TempFile(PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = TempFile(dmg.clone());
    let total = resp.content_length().unwrap_or(0);
    {
        let mut out = std::fs::File::create(&dmg)?;
        let mut stream = resp;
        let mut got: u64 = 0;
        while let Some(chunk) = stream.chunk().await? {
            use std::io::Write;
            out.write_all(&chunk)?;
            got += chunk.len() as u64;
        }
        if total > 0 && got != total {
            return Err(AppError::Other(format!(
                "the download ended early ({got} of {total} bytes) — check the connection and try again"
            )));
        }
    }
    emit(app, "qbit", "log", json!({ "message": "Installing…" }));
    let mount = std::process::Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-plist"])
        .arg(&dmg)
        .output()
        .map_err(|e| AppError::Other(format!("hdiutil: {e}")))?;
    if !mount.status.success() {
        return Err(AppError::Other(format!(
            "couldn't mount the qBittorrent disk image: {}",
            String::from_utf8_lossy(&mount.stderr).chars().take(200).collect::<String>()
        )));
    }
    let plist = String::from_utf8_lossy(&mount.stdout);
    let vol = plist
        .split("<key>mount-point</key>")
        .nth(1)
        .and_then(|s| s.split("<string>").nth(1))
        .and_then(|s| s.split("</string>").next())
        .ok_or_else(|| AppError::Other("the disk image mounted but reported no volume".into()))?
        .to_string();
    // every path below MUST unmount — RAII, not straight-line hope
    struct Mounted(String);
    impl Drop for Mounted {
        fn drop(&mut self) {
            let _ = std::process::Command::new("hdiutil")
                .args(["detach", "-force", "-quiet", &self.0])
                .output();
        }
    }
    let _vol_guard = Mounted(vol.clone());
    // find the .app by extension — don't bet on upstream's exact name/case
    let src = std::fs::read_dir(&vol)
        .ok()
        .and_then(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .find(|p| p.extension().and_then(|x| x.to_str()) == Some("app"))
        })
        .ok_or_else(|| AppError::Other("no app bundle inside the qBittorrent disk image".into()))?;
    // qBittorrent's official dmg is Developer ID signed and notarized — a
    // bundle that fails these checks is not the app we meant to install
    let verify = std::process::Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(&src)
        .output()
        .map_err(|e| AppError::Other(format!("codesign: {e}")))?;
    if !verify.status.success() {
        return Err(AppError::Other(
            "the downloaded qBittorrent failed its signature check — refusing to install it".into(),
        ));
    }
    // ditto preserves xattrs/ACLs the way Finder would; stage + move so a
    // failed copy can never leave a half-bundle that wins path resolution
    let install_into = |dest_dir: &std::path::Path| -> bool {
        let staging = dest_dir.join(".trawler-qbt-staging.app");
        let _ = std::fs::remove_dir_all(&staging);
        let ok = std::process::Command::new("ditto")
            .arg(&src)
            .arg(&staging)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_dir_all(&staging);
            return false;
        }
        let final_path = dest_dir.join("qbittorrent.app");
        let _ = std::fs::remove_dir_all(&final_path);
        std::fs::rename(&staging, &final_path).is_ok()
    };
    let mut installed = install_into(std::path::Path::new("/Applications"));
    if !installed {
        if let Some(home) = dirs::home_dir() {
            let user_apps = home.join("Applications");
            let _ = std::fs::create_dir_all(&user_apps);
            installed = install_into(&user_apps);
        }
    }
    if !installed || !qbt_exe_candidates().iter().any(|p| p.exists()) {
        return Err(AppError::Other("the qBittorrent install didn't land in Applications".into()));
    }
    emit(app, "qbit", "done", json!({ "message": "qBittorrent installed" }));
    Ok(())
}

#[cfg(any(windows, test))]
#[derive(Debug)]
struct QbtWindowsAsset {
    url: String,
    sha256: String,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug)]
struct QbtLinuxAsset {
    url: String,
    sha256: String,
}

/// Pick qBittorrent's official x86_64 AppImage. This gives Linux a portable,
/// user-scope install without guessing a distribution or invoking root.
#[cfg(any(target_os = "linux", test))]
fn qbt_linux_asset(release: &Value) -> Result<QbtLinuxAsset> {
    let assets = release
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Other("qBittorrent's release had no asset list".into()))?;
    let matches = |asset: &&Value| {
        asset
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.ends_with("_x86_64.AppImage"))
    };
    let asset = assets
        .iter()
        .filter(matches)
        .find(|asset| {
            !asset
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .contains("_lt20")
        })
        .or_else(|| assets.iter().filter(matches).next())
        .ok_or_else(|| AppError::Other("could not find qBittorrent's Linux x86_64 AppImage".into()))?;
    let url = asset
        .get("browser_download_url")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Other("qBittorrent's AppImage had no download URL".into()))?;
    if !(url.starts_with("https://github.com/qbittorrent/qBittorrent/")
        || url.starts_with("https://objects.githubusercontent.com/"))
    {
        return Err(AppError::Other(
            "qBittorrent's release URL looked wrong — not downloading it".into(),
        ));
    }
    let sha256 = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| AppError::Other("qBittorrent's AppImage had no valid SHA-256 digest".into()))?;
    Ok(QbtLinuxAsset {
        url: url.into(),
        sha256: sha256.to_ascii_lowercase(),
    })
}

/// Pick qBittorrent's normal x64 installer rather than the alternate lt20
/// build. qBittorrent does not currently publish an official Windows ARM64
/// installer, so Windows on ARM runs this payload through x64 emulation.
#[cfg(any(windows, test))]
fn qbt_windows_asset(release: &Value) -> Result<QbtWindowsAsset> {
    let assets = release
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Other("qBittorrent's release had no asset list".into()))?;
    let matches = |asset: &&Value| {
        asset
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.ends_with("_x64_setup.exe"))
    };
    let asset = assets
        .iter()
        .filter(matches)
        .find(|asset| {
            !asset
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .contains("_lt20_")
        })
        .or_else(|| assets.iter().filter(matches).next())
        .ok_or_else(|| AppError::Other("could not find qBittorrent's Windows x64 installer".into()))?;
    let url = asset
        .get("browser_download_url")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Other("qBittorrent's installer had no download URL".into()))?;
    if !(url.starts_with("https://github.com/qbittorrent/qBittorrent/")
        || url.starts_with("https://objects.githubusercontent.com/"))
    {
        return Err(AppError::Other(
            "qBittorrent's release URL looked wrong — not downloading it".into(),
        ));
    }
    let sha256 = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| AppError::Other("qBittorrent's installer had no valid SHA-256 digest".into()))?;
    Ok(QbtWindowsAsset {
        url: url.into(),
        sha256: sha256.to_ascii_lowercase(),
    })
}

#[cfg(windows)]
async fn install_qbt_windows(app: &AppHandle) -> Result<()> {
    let state_guard = app.state::<AppState>();
    let state: &AppState = state_guard.inner();

    emit(app, "qbit", "log", json!({ "message": "Finding the latest qBittorrent release…" }));
    let api = state
        .http
        .get("https://api.github.com/repos/qbittorrent/qBittorrent/releases/latest")
        .header("User-Agent", "trawler-setup")
        .send()
        .await?;
    if api.status().as_u16() == 403 {
        return Err(AppError::Other(
            "GitHub is rate-limiting release lookups from this network — try again in a few minutes".into(),
        ));
    }
    let release: Value = api.error_for_status()?.json().await?;
    let asset = qbt_windows_asset(&release)?;

    emit(app, "qbit", "log", json!({ "message": "Downloading qBittorrent from its official release…" }));
    let mut response = state
        .http
        .get(&asset.url)
        .timeout(std::time::Duration::from_secs(1800))
        .send()
        .await?
        .error_for_status()
        .map_err(|e| AppError::Other(format!("qBittorrent download failed: {e}")))?;
    let total = response.content_length().unwrap_or(0);
    let tools = local_app_data().join("TrawlerTools");
    std::fs::create_dir_all(&tools)?;
    let installer = tools.join("qbittorrent-download.exe");
    struct TempFile(PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = TempFile(installer.clone());
    let mut out = std::fs::File::create(&installer)?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    while let Some(chunk) = response.chunk().await? {
        use std::io::Write;
        out.write_all(&chunk)?;
        hasher.update(&chunk);
        downloaded += chunk.len() as u64;
    }
    out.sync_all()?;
    drop(out);
    if total > 0 && downloaded != total {
        return Err(AppError::Other(format!(
            "the download ended early ({downloaded} of {total} bytes) — check the connection and try again"
        )));
    }
    let actual_sha256 = format!("{:x}", hasher.finalize());
    if actual_sha256 != asset.sha256 {
        return Err(AppError::Other(
            "the downloaded qBittorrent installer failed its SHA-256 check — refusing to run it".into(),
        ));
    }

    emit(app, "qbit", "log", json!({ "message": "Installing qBittorrent — approve the Windows prompt if one appears…" }));
    let installer_for_task = installer.clone();
    let install = tokio::task::spawn_blocking(move || {
        // CreateProcess cannot elevate an installer by itself. Start-Process
        // supplies the UAC prompt on desktop Windows and Windows Server alike.
        let quoted = installer_for_task.to_string_lossy().replace('\'', "''");
        let script = format!(
            "$p = Start-Process -FilePath '{quoted}' -ArgumentList '/S' -Verb RunAs -Wait -PassThru; exit $p.ExitCode"
        );
        let mut cmd = std::process::Command::new("powershell.exe");
        cmd.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ]);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — UAC remains visible
        cmd.output()
    })
    .await
    .map_err(|e| AppError::Other(format!("qBittorrent installer task failed: {e}")))??;
    if !install.status.success() {
        let detail = String::from_utf8_lossy(&install.stderr);
        return Err(AppError::Other(format!(
            "qBittorrent installation was cancelled or failed: {}",
            detail.trim().chars().take(240).collect::<String>()
        )));
    }
    for _ in 0..30 {
        if qbt_exe_candidates().iter().any(|path| path.exists()) {
            emit(app, "qbit", "done", json!({ "message": "qBittorrent installed" }));
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err(AppError::Other(
        "the qBittorrent installer finished, but the application could not be found".into(),
    ))
}

/// Install qBittorrent's official AppImage under Trawler's user data folder.
/// The SHA-256 digest comes from upstream's GitHub release metadata.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
async fn install_qbt_linux(app: &AppHandle) -> Result<()> {
    let state_guard = app.state::<AppState>();
    let state: &AppState = state_guard.inner();
    emit(app, "qbit", "log", json!({ "message": "Finding the latest qBittorrent release…" }));
    let api = state
        .http
        .get("https://api.github.com/repos/qbittorrent/qBittorrent/releases/latest")
        .header("User-Agent", "trawler-setup")
        .send()
        .await?;
    if api.status().as_u16() == 403 {
        return Err(AppError::Other(
            "GitHub is rate-limiting release lookups from this network — try again in a few minutes".into(),
        ));
    }
    let release: Value = api.error_for_status()?.json().await?;
    let asset = qbt_linux_asset(&release)?;

    emit(app, "qbit", "log", json!({ "message": "Downloading qBittorrent's official AppImage…" }));
    let mut response = state
        .http
        .get(&asset.url)
        .timeout(std::time::Duration::from_secs(1800))
        .send()
        .await?
        .error_for_status()
        .map_err(|e| AppError::Other(format!("qBittorrent download failed: {e}")))?;
    let total = response.content_length().unwrap_or(0);
    let final_path = managed_qbt_appimage();
    let parent = final_path
        .parent()
        .ok_or_else(|| AppError::Other("qBittorrent's managed path had no parent".into()))?;
    std::fs::create_dir_all(parent)?;
    let staging = final_path.with_extension("AppImage.new");
    struct TempFile(PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let staging_guard = TempFile(staging.clone());
    let mut out = std::fs::File::create(&staging)?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let mut last_pct = 0u32;
    while let Some(chunk) = response.chunk().await? {
        use std::io::Write;
        out.write_all(&chunk)?;
        hasher.update(&chunk);
        downloaded += chunk.len() as u64;
        if total > 0 {
            let pct = (downloaded * 100 / total) as u32;
            if pct >= last_pct + 5 {
                last_pct = pct;
                emit(app, "qbit", "progress", json!({ "pct": pct }));
            }
        }
    }
    out.sync_all()?;
    drop(out);
    if total > 0 && downloaded != total {
        return Err(AppError::Other(format!(
            "the download ended early ({downloaded} of {total} bytes) — check the connection and try again"
        )));
    }
    if format!("{:x}", hasher.finalize()) != asset.sha256 {
        return Err(AppError::Other(
            "the downloaded qBittorrent AppImage failed its SHA-256 check — refusing to run it".into(),
        ));
    }
    {
        use std::io::Read;
        let mut magic = [0u8; 11];
        std::fs::File::open(&staging)?.read_exact(&mut magic)?;
        if magic[..4] != [0x7f, b'E', b'L', b'F'] || magic[8..11] != [b'A', b'I', 2] {
            return Err(AppError::Other(
                "the downloaded qBittorrent file was not a type-2 AppImage".into(),
            ));
        }
    }
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&staging, &final_path)?;
    std::mem::forget(staging_guard);
    emit(app, "qbit", "done", json!({ "message": "qBittorrent installed" }));
    Ok(())
}

/// Install qBittorrent through the supported path for this operating system.
pub async fn install_qbt(app: &AppHandle) -> Result<()> {
    if qbt_exe_candidates().iter().any(|path| path.exists()) {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        return install_qbt_macos(app).await;
    }
    #[cfg(windows)]
    {
        return install_qbt_windows(app).await;
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return install_qbt_linux(app).await;
    }
    #[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
    {
        let _ = app;
        return Err(AppError::Other(
            "qBittorrent does not publish an official Linux ARM AppImage yet — install qbittorrent with your distribution's package manager, then return here".into(),
        ));
    }
    #[allow(unreachable_code)]
    Err(AppError::Other("automatic qBittorrent installation is unavailable on this platform".into()))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::atomic::AtomicBool;

    use super::claim_prowlarr_lifecycle;
    use super::claim_prowlarr_lifecycle_at;
    use super::create_unique_staging_dir;
    use super::ensure_qbt_ini;
    use super::extract_windows_prowlarr;
    use super::folder_write_probe;
    use super::parse_windows_sid;
    use super::prowlarr_asset;
    use super::process_command_matches_managed;
    use super::qbt_linux_asset;
    use super::qbt_windows_asset;
    use super::replace_directory_rollback_safe;
    use super::upsert_xml_tag;
    use super::verify_extracted_files;

    fn temp_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "trawler-{label}-{}",
            super::random_api_key()
        ))
    }

    #[test]
    fn prowlarr_lifecycle_rejects_overlap_and_releases_on_drop() {
        let busy = AtomicBool::new(false);
        let first = claim_prowlarr_lifecycle(&busy).unwrap();
        assert!(claim_prowlarr_lifecycle(&busy).is_err());
        drop(first);
        assert!(claim_prowlarr_lifecycle(&busy).is_ok());
    }

    #[test]
    fn prowlarr_lifecycle_file_lock_rejects_a_second_process() {
        let root = temp_path("prowlarr-lifecycle-lock");
        let lock_path = root.join("lifecycle.lock");
        let first_process_flag = AtomicBool::new(false);
        let second_process_flag = AtomicBool::new(false);
        let first = claim_prowlarr_lifecycle_at(&first_process_flag, &lock_path).unwrap();
        assert!(claim_prowlarr_lifecycle_at(&second_process_flag, &lock_path).is_err());
        drop(first);
        assert!(claim_prowlarr_lifecycle_at(&second_process_flag, &lock_path).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_process_match_requires_our_executable_and_data_paths() {
        let exe = std::path::Path::new("/managed/TrawlerTools/Prowlarr/Prowlarr");
        let data = std::path::Path::new("/managed/TrawlerTools/ProwlarrData");
        assert!(process_command_matches_managed(
            "/managed/TrawlerTools/Prowlarr/Prowlarr -data=/managed/TrawlerTools/ProwlarrData",
            exe,
            data,
        ));
        assert!(!process_command_matches_managed(
            "/user/Prowlarr/Prowlarr -data=/user/ProwlarrData",
            exe,
            data,
        ));
        assert!(!process_command_matches_managed(
            "/managed/TrawlerTools/Prowlarr/Prowlarr -data=/user/ProwlarrData",
            exe,
            data,
        ));
    }

    #[test]
    fn selects_prowlarr_asset_only_with_trusted_url_and_digest() {
        let release = serde_json::json!({
            "assets": [{
                "name": "Prowlarr.master.2.5.2.5491.windows-core-x64.zip",
                "digest": format!("sha256:{}", "a".repeat(64)),
                "browser_download_url": "https://github.com/Prowlarr/Prowlarr/releases/download/v2.5.2.5491/Prowlarr.master.2.5.2.5491.windows-core-x64.zip"
            }]
        });
        let asset = prowlarr_asset(&release, "windows-core-x64.zip").unwrap();
        assert_eq!(asset.sha256, "a".repeat(64));

        let mut bad_digest = release.clone();
        bad_digest["assets"][0]["digest"] = serde_json::json!("sha256:not-valid");
        assert!(prowlarr_asset(&bad_digest, "windows-core-x64.zip").is_err());

        let mut bad_host = release;
        bad_host["assets"][0]["browser_download_url"] =
            serde_json::json!("https://example.invalid/Prowlarr.zip");
        assert!(prowlarr_asset(&bad_host, "windows-core-x64.zip").is_err());
    }

    #[test]
    fn staging_directories_are_unique_siblings() {
        let root = temp_path("prowlarr-staging");
        let final_target = root.join("Prowlarr");
        let first = create_unique_staging_dir(&final_target).unwrap();
        let second = create_unique_staging_dir(&final_target).unwrap();
        assert_ne!(first, second);
        assert_eq!(first.parent(), final_target.parent());
        assert!(first.is_dir() && second.is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn windows_zip_extraction_writes_every_archived_file() {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default();
        archive.add_directory("Prowlarr/", options).unwrap();
        archive.start_file("Prowlarr/Prowlarr.exe", options).unwrap();
        archive.write_all(b"exe").unwrap();
        archive.start_file("Prowlarr/AngleSharp.dll", options).unwrap();
        archive.write_all(b"anglesharp").unwrap();
        let bytes = archive.finish().unwrap().into_inner();

        let target = temp_path("prowlarr-extract");
        std::fs::create_dir_all(&target).unwrap();
        extract_windows_prowlarr(bytes, &target).unwrap();
        assert_eq!(std::fs::read(target.join("Prowlarr.exe")).unwrap(), b"exe");
        assert_eq!(
            std::fs::read(target.join("AngleSharp.dll")).unwrap(),
            b"anglesharp"
        );
        std::fs::remove_dir_all(target).unwrap();
    }

    #[test]
    fn extracted_manifest_rejects_missing_or_truncated_files() {
        let target = temp_path("prowlarr-manifest");
        std::fs::create_dir_all(&target).unwrap();
        let manifest = vec![(std::path::PathBuf::from("AngleSharp.dll"), 10)];
        assert!(verify_extracted_files(&target, &manifest).is_err());
        std::fs::write(target.join("AngleSharp.dll"), b"short").unwrap();
        assert!(verify_extracted_files(&target, &manifest).is_err());
        std::fs::write(target.join("AngleSharp.dll"), b"0123456789").unwrap();
        assert!(verify_extracted_files(&target, &manifest).is_ok());
        std::fs::remove_dir_all(target).unwrap();
    }

    #[test]
    fn directory_activation_replaces_complete_tree_and_removes_backup() {
        let root = temp_path("prowlarr-activate");
        let final_target = root.join("Prowlarr");
        let staged = root.join("stage");
        std::fs::create_dir_all(&final_target).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(final_target.join("old.dll"), b"old").unwrap();
        std::fs::write(staged.join("new.dll"), b"new").unwrap();

        let activation = replace_directory_rollback_safe(&staged, &final_target).unwrap();
        let backup = activation.backup_path().unwrap().to_path_buf();
        assert!(!final_target.join("old.dll").exists());
        assert_eq!(std::fs::read(final_target.join("new.dll")).unwrap(), b"new");
        assert_eq!(std::fs::read(backup.join("old.dll")).unwrap(), b"old");
        activation.commit();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_readiness_keeps_previous_executable_tree() {
        let root = temp_path("prowlarr-readiness-failure");
        let final_target = root.join("Prowlarr");
        let staged = root.join("stage");
        std::fs::create_dir_all(&final_target).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(final_target.join("old.dll"), b"old").unwrap();
        std::fs::write(staged.join("new.dll"), b"new").unwrap();

        let activation = replace_directory_rollback_safe(&staged, &final_target).unwrap();
        let backup = activation.backup_path().unwrap().to_path_buf();
        drop(activation); // simulates config/readiness failing before commit
        assert_eq!(std::fs::read(backup.join("old.dll")).unwrap(), b"old");
        assert_eq!(std::fs::read(final_target.join("new.dll")).unwrap(), b"new");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_activation_restores_old_tree_when_staged_rename_fails() {
        let root = temp_path("prowlarr-rollback");
        let final_target = root.join("Prowlarr");
        let staged = final_target.join("stage");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(final_target.join("old.dll"), b"old").unwrap();
        std::fs::write(staged.join("new.dll"), b"new").unwrap();

        let error = replace_directory_rollback_safe(&staged, &final_target).unwrap_err();
        assert!(error.to_string().contains("previous install was restored"));
        assert_eq!(std::fs::read(final_target.join("old.dll")).unwrap(), b"old");
        assert!(final_target.join("stage/new.dll").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn write_probe_checks_create_write_and_delete() {
        let path = std::env::temp_dir().join(format!(
            "trawler-prowlarr-write-probe-{}",
            super::random_api_key()
        ));
        folder_write_probe(&path).unwrap();
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        std::fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn parses_only_valid_windows_user_sids() {
        let output = "\"ARMBOX\\dev\",\"S-1-5-21-123-456-789-1001\"\r\n";
        assert_eq!(
            parse_windows_sid(output).as_deref(),
            Some("S-1-5-21-123-456-789-1001")
        );
        assert_eq!(parse_windows_sid("dev,S-1-5-21-1 & whoami"), None);
        assert_eq!(parse_windows_sid("unexpected output"), None);
    }

    #[test]
    fn selects_normal_qbt_windows_installer_with_digest() {
        let release = serde_json::json!({
            "assets": [
                {
                    "name": "qbittorrent_5.2.3_lt20_x64_setup.exe",
                    "digest": format!("sha256:{}", "a".repeat(64)),
                    "browser_download_url": "https://github.com/qbittorrent/qBittorrent/releases/download/release-5.2.3/qbittorrent_5.2.3_lt20_x64_setup.exe"
                },
                {
                    "name": "qbittorrent_5.2.3_x64_setup.exe",
                    "digest": format!("sha256:{}", "b".repeat(64)),
                    "browser_download_url": "https://github.com/qbittorrent/qBittorrent/releases/download/release-5.2.3/qbittorrent_5.2.3_x64_setup.exe"
                }
            ]
        });
        let asset = qbt_windows_asset(&release).unwrap();
        assert!(asset.url.ends_with("qbittorrent_5.2.3_x64_setup.exe"));
        assert_eq!(asset.sha256, "b".repeat(64));
    }

    #[test]
    fn rejects_qbt_installer_without_a_valid_digest() {
        let release = serde_json::json!({
            "assets": [{
                "name": "qbittorrent_5.2.3_x64_setup.exe",
                "digest": "sha256:not-a-digest",
                "browser_download_url": "https://github.com/qbittorrent/qBittorrent/releases/download/release-5.2.3/qbittorrent_5.2.3_x64_setup.exe"
            }]
        });
        assert!(qbt_windows_asset(&release).is_err());
    }

    #[test]
    fn selects_normal_qbt_linux_appimage_with_digest() {
        let release = serde_json::json!({
            "assets": [
                {
                    "name": "qbittorrent-5.2.3_lt20_x86_64.AppImage",
                    "digest": format!("sha256:{}", "a".repeat(64)),
                    "browser_download_url": "https://github.com/qbittorrent/qBittorrent/releases/download/release-5.2.3/qbittorrent-5.2.3_lt20_x86_64.AppImage"
                },
                {
                    "name": "qbittorrent-5.2.3_x86_64.AppImage",
                    "digest": format!("sha256:{}", "b".repeat(64)),
                    "browser_download_url": "https://github.com/qbittorrent/qBittorrent/releases/download/release-5.2.3/qbittorrent-5.2.3_x86_64.AppImage"
                }
            ]
        });
        let asset = qbt_linux_asset(&release).unwrap();
        assert!(asset.url.ends_with("qbittorrent-5.2.3_x86_64.AppImage"));
        assert_eq!(asset.sha256, "b".repeat(64));
    }

    #[test]
    fn rejects_qbt_linux_appimage_from_an_untrusted_host() {
        let release = serde_json::json!({
            "assets": [{
                "name": "qbittorrent-5.2.3_x86_64.AppImage",
                "digest": format!("sha256:{}", "b".repeat(64)),
                "browser_download_url": "https://example.invalid/qbittorrent.AppImage"
            }]
        });
        assert!(qbt_linux_asset(&release).is_err());
    }

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
