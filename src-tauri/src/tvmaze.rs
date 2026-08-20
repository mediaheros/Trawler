//! TVmaze API client — free, keyless. https://api.tvmaze.com

use serde::{Deserialize, Serialize};
use crate::error::{AppError, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct TvmSearchHit {
    pub show: TvmShow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TvmShow {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub premiered: Option<String>,
    pub ended: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    pub network: Option<TvmNetwork>,
    #[serde(rename = "webChannel")]
    pub web_channel: Option<TvmNetwork>,
    pub image: Option<TvmImage>,
    pub externals: Option<TvmExternals>,
    pub summary: Option<String>,
    #[serde(rename = "_embedded")]
    pub embedded: Option<TvmEmbedded>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TvmNetwork {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TvmImage {
    pub medium: Option<String>,
    pub original: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TvmExternals {
    pub imdb: Option<String>,
    pub thetvdb: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TvmEmbedded {
    #[serde(default)]
    pub episodes: Vec<TvmEpisode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TvmEpisode {
    pub id: i64,
    pub season: i64,
    pub number: Option<i64>,
    pub name: Option<String>,
    pub airstamp: Option<String>,
    #[serde(rename = "type", default)]
    pub ep_type: Option<String>,
}

impl TvmShow {
    pub fn network_name(&self) -> Option<String> {
        self.network
            .as_ref()
            .or(self.web_channel.as_ref())
            .map(|n| n.name.clone())
    }
    pub fn poster(&self) -> Option<String> {
        self.image
            .as_ref()
            .and_then(|i| i.medium.clone().or_else(|| i.original.clone()))
    }
}

const BASE: &str = "https://api.tvmaze.com";

/// TVmaze allows ~20 requests per 10 seconds ACROSS the whole app. Every call
/// funnels through this gate: callers serialize on the mutex and each waits
/// out the minimum gap, so import bursts, discover rows and follow refreshes
/// collectively stay under the budget.
static GATE: std::sync::OnceLock<tokio::sync::Mutex<std::time::Instant>> = std::sync::OnceLock::new();
const MIN_GAP: std::time::Duration = std::time::Duration::from_millis(550);

async fn rate_gate() {
    let gate = GATE.get_or_init(|| {
        tokio::sync::Mutex::new(std::time::Instant::now() - MIN_GAP)
    });
    let mut last = gate.lock().await;
    let elapsed = last.elapsed();
    if elapsed < MIN_GAP {
        tokio::time::sleep(MIN_GAP - elapsed).await;
    }
    *last = std::time::Instant::now();
}

/// Gated GET for arbitrary TVmaze URLs (discover schedules, previews) —
/// same politeness rules as the typed endpoints.
pub async fn get_gated(http: &reqwest::Client, url: &str) -> Result<reqwest::Response> {
    get_with_retry(http, url, &[]).await
}

/// One polite retry on 429, honoring Retry-After (capped at 15s).
async fn get_with_retry(http: &reqwest::Client, url: &str, query: &[(&str, &str)]) -> Result<reqwest::Response> {
    for attempt in 0..2 {
        rate_gate().await;
        let resp = http.get(url).query(query).send().await?;
        if resp.status().as_u16() == 429 && attempt == 0 {
            let wait = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(5)
                .min(15);
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            continue;
        }
        return Ok(resp);
    }
    unreachable!("retry loop always returns")
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(AppError::Other(format!(
        "TVmaze error {}: {}",
        status.as_u16(),
        body.chars().take(200).collect::<String>()
    )))
}

pub async fn search_shows(http: &reqwest::Client, query: &str) -> Result<Vec<TvmShow>> {
    let resp = get_with_retry(http, &format!("{BASE}/search/shows"), &[("q", query)]).await?;
    let hits: Vec<TvmSearchHit> = check(resp).await?.json().await?;
    Ok(hits.into_iter().map(|h| h.show).collect())
}

/// Show with its full episode list embedded.
pub async fn show_with_episodes(http: &reqwest::Client, id: i64) -> Result<TvmShow> {
    let resp = get_with_retry(http, &format!("{BASE}/shows/{id}"), &[("embed", "episodes")]).await?;
    Ok(check(resp).await?.json().await?)
}
