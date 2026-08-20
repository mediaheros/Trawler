//! Outbound notifications: Discord webhooks and Telegram bots.
//! Strictly fire-and-forget — a slow or broken webhook must never block or
//! fail a grab path, so every send happens on a spawned task with its own
//! timeout, and failures go to the log, not to the user's activity feed.

use std::sync::Mutex;

use tauri::Manager;

use crate::config::Config;
use crate::error::{AppError, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Grab,
    Complete,
    Proposal,
    Error,
}

impl Kind {
    fn discord_color(self) -> u32 {
        match self {
            Kind::Grab => 0x2D_D4BF,     // accent teal
            Kind::Complete => 0x4A_DE80, // ok green
            Kind::Proposal => 0xFB_BF24, // amber
            Kind::Error => 0xF8_7171,    // red
        }
    }

    fn emoji(self) -> &'static str {
        match self {
            Kind::Grab => "\u{2B07}\u{FE0F}",     // ⬇️
            Kind::Complete => "\u{2705}",         // ✅
            Kind::Proposal => "\u{1F4A1}",        // 💡
            Kind::Error => "\u{26A0}\u{FE0F}",    // ⚠️
        }
    }

    fn enabled(self, cfg: &Config) -> bool {
        match self {
            Kind::Grab => cfg.notify_grabs,
            Kind::Complete => cfg.notify_completions,
            Kind::Proposal => cfg.notify_proposals,
            Kind::Error => cfg.notify_errors,
        }
    }
}

/// Timestamps of recent sends; a runaway loop must not machine-gun a phone.
static SENT: Mutex<Vec<i64>> = Mutex::new(Vec::new());
const MAX_PER_HOUR: usize = 30;

fn rate_ok() -> bool {
    let now = crate::db::now();
    let mut sent = match SENT.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };
    sent.retain(|t| now - *t < 3600);
    if sent.len() >= MAX_PER_HOUR {
        return false;
    }
    sent.push(now);
    true
}

fn configured(cfg: &Config) -> (bool, bool) {
    let discord = !cfg.discord_webhook.trim().is_empty();
    let telegram =
        !cfg.telegram_bot_token.trim().is_empty() && !cfg.telegram_chat_id.trim().is_empty();
    (discord, telegram)
}

/// Queue a notification. Returns immediately; delivery is best-effort.
pub fn dispatch(app: &tauri::AppHandle, kind: Kind, title: String, body: String) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let cfg = {
            let state = app.state::<crate::AppState>();
            let cfg = state.config.read().await.clone();
            cfg
        };
        let (discord, telegram) = configured(&cfg);
        if (!discord && !telegram) || !kind.enabled(&cfg) {
            return;
        }
        if !rate_ok() {
            eprintln!("[trawler] notification suppressed (over {MAX_PER_HOUR}/hour): {title}");
            return;
        }
        let http = {
            let state = app.state::<crate::AppState>();
            state.http.clone()
        };
        if discord {
            if let Err(e) = send_discord(&http, &cfg.discord_webhook, kind, &title, &body).await {
                eprintln!("[trawler] discord notification failed: {e}");
            }
        }
        if telegram {
            if let Err(e) = send_telegram(
                &http,
                &cfg.telegram_bot_token,
                &cfg.telegram_chat_id,
                kind,
                &title,
                &body,
            )
            .await
            {
                eprintln!("[trawler] telegram notification failed: {e}");
            }
        }
    });
}

const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn send_discord(
    http: &reqwest::Client,
    webhook: &str,
    kind: Kind,
    title: &str,
    body: &str,
) -> Result<()> {
    let payload = serde_json::json!({
        "username": "Trawler",
        "embeds": [{
            "title": format!("{} {}", kind.emoji(), title.chars().take(240).collect::<String>()),
            "description": body.chars().take(2000).collect::<String>(),
            "color": kind.discord_color(),
        }],
    });
    // transport errors would otherwise print the webhook URL — the credential
    let resp = http
        .post(webhook.trim())
        .timeout(SEND_TIMEOUT)
        .json(&payload)
        .send()
        .await
        .map_err(|e| AppError::Other(format!("Discord: {}", e.without_url())))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::Other(format!(
            "Discord returned {status}: {}",
            text.chars().take(200).collect::<String>()
        )));
    }
    Ok(())
}

async fn send_telegram(
    http: &reqwest::Client,
    token: &str,
    chat_id: &str,
    kind: Kind,
    title: &str,
    body: &str,
) -> Result<()> {
    // plain text on purpose: release names are full of Markdown-hostile characters
    let text = format!(
        "{} {}\n{}",
        kind.emoji(),
        title.chars().take(240).collect::<String>(),
        body.chars().take(3500).collect::<String>()
    );
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token.trim());
    // transport errors would otherwise print the bot-token URL
    let resp = http
        .post(&url)
        .timeout(SEND_TIMEOUT)
        .json(&serde_json::json!({ "chat_id": chat_id.trim(), "text": text }))
        .send()
        .await
        .map_err(|e| AppError::Other(format!("Telegram: {}", e.without_url())))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        // Telegram error bodies are JSON with a helpful "description"
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("description").and_then(|d| d.as_str()).map(String::from))
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(AppError::Other(format!("Telegram returned {status}: {detail}")));
    }
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestOutcome {
    /// None = channel not configured; Some(Ok-ish string) or Some(error text)
    pub discord: Option<String>,
    pub discord_ok: bool,
    pub telegram: Option<String>,
    pub telegram_ok: bool,
}

/// Send a test message to every configured channel, reporting per-channel
/// results. Ignores the event toggles — the user explicitly asked.
pub async fn send_test(http: &reqwest::Client, cfg: &Config) -> TestOutcome {
    let (discord, telegram) = configured(cfg);
    let title = "Test notification";
    let body = "Trawler can reach this channel. You're all set.";

    let (discord_res, telegram_res) = tokio::join!(
        async {
            if !discord {
                return None;
            }
            Some(send_discord(http, &cfg.discord_webhook, Kind::Complete, title, body).await)
        },
        async {
            if !telegram {
                return None;
            }
            Some(
                send_telegram(
                    http,
                    &cfg.telegram_bot_token,
                    &cfg.telegram_chat_id,
                    Kind::Complete,
                    title,
                    body,
                )
                .await,
            )
        }
    );

    let fold = |r: Option<crate::error::Result<()>>| match r {
        None => (None, false),
        Some(Ok(())) => (Some("Delivered".to_string()), true),
        Some(Err(e)) => (Some(e.to_string()), false),
    };
    let (discord, discord_ok) = fold(discord_res);
    let (telegram, telegram_ok) = fold(telegram_res);
    TestOutcome { discord, discord_ok, telegram, telegram_ok }
}
