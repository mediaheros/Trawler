use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct QualityProfile {
    /// hard filter; empty = anything. Unknown-resolution releases always pass.
    pub resolutions: Vec<String>,
    /// "prefer-x265" | "prefer-x264" | "any" — a score boost, not a hard filter
    pub codec: String,
    /// per-episode size cap in GB; 0 = unlimited. Season packs get cap × episodes.
    pub max_size_gb: f64,
    pub allow_season_packs: bool,
}

impl Default for QualityProfile {
    fn default() -> Self {
        Self {
            resolutions: vec!["1080p".into(), "720p".into()],
            codec: "prefer-x265".into(),
            max_size_gb: 0.0,
            allow_season_packs: true,
        }
    }
}

/// A named, reusable quality profile shown as one-click chips in the UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct QualityPreset {
    pub name: String,
    pub profile: QualityProfile,
}

pub fn default_presets() -> Vec<QualityPreset> {
    let preset = |name: &str, resolutions: &[&str], max_size_gb: f64| QualityPreset {
        name: name.into(),
        profile: QualityProfile {
            resolutions: resolutions.iter().map(|s| s.to_string()).collect(),
            codec: "prefer-x265".into(),
            max_size_gb,
            allow_season_packs: true,
        },
    };
    vec![
        preset("Best available", &["2160p", "1080p"], 0.0),
        preset("1080p quality", &["1080p"], 8.0),
        preset("1080p balanced", &["1080p", "720p"], 3.0),
        preset("Space saver", &["720p"], 1.5),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub prowlarr_url: String,
    pub prowlarr_api_key: String,
    pub qbit_url: String,
    pub qbit_username: String,
    pub qbit_password: String,
    /// qBittorrent category to tag grabs with (created on demand)
    pub qbit_category: String,
    /// what happens after a grab completes: "default" (qBittorrent's own
    /// settings), "none" (stop as soon as complete), "ratio" (stop at seed_ratio)
    pub seed_policy: String,
    pub seed_ratio: f64,
    /// Optional save-path overrides per kind ("movies" / "tv")
    pub save_path_movies: String,
    pub save_path_tv: String,
    pub add_paused: bool,
    /// default quality profile for followed shows (per-show overrides exist)
    pub default_quality: QualityProfile,
    /// named one-click profiles; user-editable, seeded with sensible defaults
    pub quality_presets: Vec<QualityPreset>,
    /// how often the follow scheduler wakes up
    pub scheduler_minutes: u32,
    /// fire a Windows toast when the scheduler grabs something
    pub notify_on_grab: bool,
    /// Discord webhook URL for push notifications (empty = off)
    pub discord_webhook: String,
    /// Telegram bot credentials (both required for the channel to be on)
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    /// which events reach Discord/Telegram
    pub notify_grabs: bool,
    pub notify_completions: bool,
    pub notify_proposals: bool,
    pub notify_errors: bool,
    /// closing the window hides to tray so the scheduler keeps running
    pub close_to_tray: bool,
    /// OpenAI-compatible endpoint for the agent (Ollama)
    pub agent_base_url: String,
    pub agent_model: String,
    pub agent_enabled: bool,
    /// refuse agent grabs when download disk free space falls below this (GB)
    pub agent_min_free_disk_gb: f64,
    /// dead-swarm replacement: "off" | "propose" | "auto"
    pub medic_mode: String,
    /// first-run wizard finished (or skipped)
    pub setup_completed: bool,
    /// Sonarr-style latest-releases sweep: matches new uploads against wanted
    /// episodes and briefs within minutes of posting
    pub rss_enabled: bool,
    pub rss_minutes: u32,
    /// where grabs go: "qbittorrent" (local, default) or "bitport" (cloud)
    pub download_backend: String,
    /// Bitport bearer token (empty = not connected); the connect flow mints it
    pub bitport_token: String,
    /// weekly propose-only re-search for better-quality copies of recent grabs
    pub upgrade_scout_enabled: bool,
    /// how far back a download still counts as "recent" for the scout (days)
    pub upgrade_window_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prowlarr_url: "http://127.0.0.1:9696".into(),
            prowlarr_api_key: String::new(),
            qbit_url: "http://127.0.0.1:8080".into(),
            qbit_username: String::new(),
            qbit_password: String::new(),
            qbit_category: "trawler".into(),
            seed_policy: "default".into(),
            seed_ratio: 1.0,
            save_path_movies: String::new(),
            save_path_tv: String::new(),
            add_paused: false,
            default_quality: QualityProfile::default(),
            quality_presets: default_presets(),
            scheduler_minutes: 30,
            notify_on_grab: true,
            discord_webhook: String::new(),
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            notify_grabs: true,
            notify_completions: true,
            notify_proposals: true,
            notify_errors: false,
            close_to_tray: true,
            agent_base_url: "http://127.0.0.1:11434".into(),
            agent_model: "kimi-k2.6:cloud".into(),
            agent_enabled: true,
            agent_min_free_disk_gb: 50.0,
            medic_mode: "propose".into(),
            setup_completed: false,
            rss_enabled: true,
            rss_minutes: 15,
            download_backend: "qbittorrent".into(),
            bitport_token: String::new(),
            upgrade_scout_enabled: false,
            upgrade_window_days: 30,
        }
    }
}

pub fn config_path() -> PathBuf {
    // %APPDATA% on Windows (same path as before — no migration),
    // ~/.config on Linux, ~/Library/Application Support on macOS
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("trawler").join("config.json")
}

/// The app directory contains credentials, agent conversations, private
/// tracker URLs, and SQLite WAL files. Restrict the directory itself so even
/// an old database created with a permissive umask is not traversable by
/// other local users.
pub fn ensure_private_app_dir(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn secure_existing_config_artifacts(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let Some(dir) = path.parent() else {
        return Ok(());
    };
    let config_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("config.json");
    let stem = path.file_stem().and_then(|name| name.to_str()).unwrap_or("config");
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_secret_artifact = name == config_name
            || name.starts_with(&format!("{config_name}.tmp"))
            || name == format!("{stem}.tmp")
            || name.starts_with(&format!("{stem}.tmp."))
            || name.starts_with(&format!("{stem}.corrupt-"))
            || name.starts_with(&format!("{config_name}.corrupt-"));
        if is_secret_artifact {
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_existing_config_artifacts(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

fn load_from_path(path: &std::path::Path) -> Config {
    if let Some(dir) = path.parent() {
        if let Err(error) = ensure_private_app_dir(dir) {
            crate::applog::error(
                "app",
                format!("could not secure the Trawler data directory: {error}"),
            );
        }
    }
    // Repair permissions before reading or copying any secret-bearing file.
    if let Err(error) = secure_existing_config_artifacts(path) {
        crate::applog::error(
            "app",
            format!("could not secure existing Trawler configuration files: {error}"),
        );
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => Some(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            // a file that exists but cannot be read right now (permissions,
            // a sharing violation, a roaming profile mid-sync) is not "no
            // config": starting with defaults would make the setup wizard
            // save over it and destroy every stored key and token
            CONFIG_READ_FAILED.store(true, std::sync::atomic::Ordering::SeqCst);
            crate::applog::error(
                "app",
                format!(
                    "config.json exists but could not be read ({e}); running with defaults and \
                     refusing to save settings until Trawler is restarted, so the file is not overwritten"
                ),
            );
            return Config::default();
        }
    };
    let parsed = raw.as_deref().map(serde_json::from_str::<Config>);
    match parsed {
        Some(Ok(c)) => c,
        Some(Err(e)) => {
            // an unparseable config must never be silently replaced by
            // defaults — keep it so the user's keys can be recovered
            let backup = path.with_extension(format!(
                "corrupt-{}-{}.json",
                crate::db::now(),
                std::process::id()
            ));
            let backup_saved = raw
                .as_deref()
                .map(|raw| write_private_file(&backup, raw.as_bytes()))
                .transpose();
            let recovery = match backup_saved {
                Ok(Some(())) => format!("kept a private copy at {}", backup.display()),
                Ok(None) => "the original file remains in place".into(),
                Err(error) => format!(
                    "could not create a backup ({error}); the original file remains at {}",
                    path.display()
                ),
            };
            crate::applog::error(
                "app",
                format!(
                    "config.json could not be read ({e}); {recovery}; started with defaults"
                ),
            );
            Config::default()
        }
        None => Config::default(),
    }
}

pub fn load() -> Config {
    let path = config_path();
    let mut cfg = load_from_path(&path);
    // migration: an install configured before the wizard existed counts as set up
    if !cfg.setup_completed && !cfg.prowlarr_api_key.is_empty() {
        cfg.setup_completed = true;
        let _ = save(&cfg);
    }
    cfg
}

/// Set when config.json exists but could not be read at startup. While set,
/// `save` refuses to run: the in-memory config is defaults, and writing it
/// would silently replace the user's real settings and secrets.
static CONFIG_READ_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn save(cfg: &Config) -> Result<()> {
    if CONFIG_READ_FAILED.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(crate::error::AppError::Other(
            "config.json could not be read when Trawler started, so settings are not being saved \
             to avoid overwriting it — restart Trawler and try again"
                .into(),
        ));
    }
    // two concurrent saves must not interleave writes to one tmp file —
    // the rename would make the corruption permanent
    static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SAVE_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let path = config_path();
    if let Some(dir) = path.parent() {
        ensure_private_app_dir(dir)?;
    }
    // atomic: a crash mid-write must not truncate the user's settings. The
    // tmp name carries the pid so a crashed save can't collide with a later
    // one (a stale tmp is litter, never corruption).
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    {
        let json = serde_json::to_string_pretty(cfg)?;
        write_private_file(&tmp, json.as_bytes())?;
    }
    replace_file(&tmp, &path)?;
    Ok(())
}

/// Atomic, owner-only write for any secret-bearing file (Prowlarr's
/// config.xml, qBittorrent's ini): a crash mid-write must leave the previous
/// file intact, never a truncated one that breaks the service on next start.
pub(crate) fn write_atomic_private(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    // two overlapping writers (a double-clicked "Enable Web UI") would share
    // one pid-named tmp and publish each other's half-written file
    static ATOMIC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = ATOMIC_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = path.with_extension(format!(
        "{}.tmp.{}",
        path.extension().and_then(|e| e.to_str()).unwrap_or("dat"),
        std::process::id()
    ));
    write_private_file(&tmp, contents)?;
    replace_file(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn write_private_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // `mode` only applies when the file is first created. A stale temp file
    // from a crashed save may already exist with broader permissions.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    use std::io::Write;
    file.write_all(contents)?;
    file.sync_all()
}

/// Atomically replace an existing config. POSIX rename overwrites; Windows'
/// standard-library rename does not, so use the native replace flag there.
#[cfg(not(windows))]
fn replace_file(source: &std::path::Path, destination: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &std::path::Path, destination: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{load_from_path, replace_file, write_private_file};

    #[test]
    fn atomic_replace_overwrites_an_existing_file() {
        let root = std::env::temp_dir().join(format!(
            "trawler-config-replace-{}-{}",
            std::process::id(),
            crate::db::now()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let destination = root.join("config.json");
        let source = root.join("config.tmp");
        std::fs::write(&destination, b"old").unwrap();
        std::fs::write(&source, b"new").unwrap();

        replace_file(&source, &destination).unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
        assert!(!source.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn config_temp_file_is_owner_only_even_when_reused() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "trawler-config-private-{}-{}",
            std::process::id(),
            crate::db::now()
        ));
        std::fs::write(&path, b"stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private_file(&path, b"secret").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"secret");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn startup_repairs_existing_secret_artifact_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "trawler-config-upgrade-{}-{}",
            std::process::id(),
            crate::db::now()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = root.join("config.json");
        let artifacts = [
            config.clone(),
            root.join("config.json.tmp"),
            root.join("config.tmp"),
            root.join("config.corrupt-old.json"),
        ];
        for artifact in &artifacts {
            std::fs::write(artifact, b"{").unwrap();
            std::fs::set_permissions(artifact, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let _ = load_from_path(&config);

        assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode() & 0o777, 0o700);
        for entry in std::fs::read_dir(&root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                assert_eq!(
                    entry.metadata().unwrap().permissions().mode() & 0o777,
                    0o600,
                    "{} was not repaired",
                    entry.path().display()
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
