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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct QualityPreset {
    pub name: String,
    pub profile: QualityProfile,
}

impl Default for QualityPreset {
    fn default() -> Self {
        Self { name: String::new(), profile: QualityProfile::default() }
    }
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

pub fn load() -> Config {
    let path = config_path();
    let raw = std::fs::read_to_string(&path).ok();
    let parsed = raw.as_deref().map(|s| serde_json::from_str::<Config>(s));
    let mut cfg: Config = match parsed {
        Some(Ok(c)) => c,
        Some(Err(e)) => {
            // an unparseable config must never be silently replaced by
            // defaults — keep it so the user's keys can be recovered
            let backup = path.with_extension(format!("corrupt-{}.json", crate::db::now()));
            let _ = std::fs::copy(&path, &backup);
            crate::applog::error(
                "app",
                format!(
                    "config.json could not be read ({e}); kept a copy at {} and started with defaults",
                    backup.display()
                ),
            );
            Config::default()
        }
        None => Config::default(),
    };
    // migration: an install configured before the wizard existed counts as set up
    if !cfg.setup_completed && !cfg.prowlarr_api_key.is_empty() {
        cfg.setup_completed = true;
        let _ = save(&cfg);
    }
    cfg
}

pub fn save(cfg: &Config) -> Result<()> {
    // two concurrent saves must not interleave writes to one tmp file —
    // the rename would make the corruption permanent
    static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SAVE_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // atomic: a crash mid-write must not truncate the user's settings. The
    // tmp name carries the pid so a crashed save can't collide with a later
    // one (a stale tmp is litter, never corruption).
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    {
        let json = serde_json::to_string_pretty(cfg)?;
        let mut f = std::fs::File::create(&tmp)?;
        use std::io::Write;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
