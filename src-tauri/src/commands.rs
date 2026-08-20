use std::collections::HashMap;

use serde::Serialize;
use tauri::State;

use crate::config::{self, Config};
use crate::error::{AppError, Result};
use crate::parse::{self, ParsedRelease};
use crate::prowlarr::{ProwlarrClient, ProwlarrIndexer, ProwlarrRelease};
use crate::qbit::{AddTorrent, QbitClient, QbitTorrent, TransferInfo};
use crate::AppState;

pub const CAT_MOVIES: i32 = 2000;
pub const CAT_TV: i32 = 5000;

fn prowlarr<'a>(http: &'a reqwest::Client, cfg: &Config) -> Result<ProwlarrClient<'a>> {
    if cfg.prowlarr_api_key.is_empty() {
        return Err(AppError::NotConfigured("Prowlarr"));
    }
    Ok(ProwlarrClient {
        http,
        base: cfg.prowlarr_url.clone(),
        api_key: cfg.prowlarr_api_key.clone(),
    })
}

/// The Prowlarr client for other command modules (setup's unlock flow).
pub(crate) fn prowlarr_pub<'a>(
    http: &'a reqwest::Client,
    cfg: &Config,
) -> Result<crate::prowlarr::ProwlarrClient<'a>> {
    prowlarr(http, cfg)
}

fn qbit<'a>(http: &'a reqwest::Client, cfg: &Config) -> QbitClient<'a> {
    QbitClient {
        http,
        base: cfg.qbit_url.clone(),
        username: cfg.qbit_username.clone(),
        password: cfg.qbit_password.clone(),
    }
}

// ---------- config ----------

#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<Config> {
    Ok(state.config.read().await.clone())
}

#[tauri::command]
pub async fn set_config(state: State<'_, AppState>, config: Config) -> Result<Config> {
    config::save(&config)?;
    *state.config.write().await = config.clone();
    Ok(config)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionStatus {
    pub prowlarr_ok: bool,
    pub prowlarr_detail: String,
    pub qbit_ok: bool,
    pub qbit_detail: String,
}

#[tauri::command]
pub async fn test_connections(state: State<'_, AppState>) -> Result<ConnectionStatus> {
    let cfg = state.config.read().await.clone();

    let (prowlarr_ok, prowlarr_detail) = match prowlarr(&state.http, &cfg) {
        Err(e) => (false, e.to_string()),
        Ok(client) => match client.ping().await {
            Ok(v) => (
                true,
                format!(
                    "Prowlarr {}",
                    v.get("version").and_then(|x| x.as_str()).unwrap_or("?")
                ),
            ),
            Err(e) => (false, e.to_string()),
        },
    };

    let q = qbit(&state.http, &cfg);
    let (qbit_ok, qbit_detail) = match q.version().await {
        Ok(v) => (true, format!("qBittorrent {v}")),
        Err(e) => (false, e.to_string()),
    };

    Ok(ConnectionStatus { prowlarr_ok, prowlarr_detail, qbit_ok, qbit_detail })
}

#[tauri::command]
pub async fn list_indexers(state: State<'_, AppState>) -> Result<Vec<ProwlarrIndexer>> {
    let cfg = state.config.read().await.clone();
    prowlarr(&state.http, &cfg)?.indexers().await
}

// ---------- search ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichedRelease {
    #[serde(flatten)]
    pub release: ProwlarrRelease,
    pub parsed: ParsedRelease,
    pub score: f64,
    /// how many identical copies (same infohash / same normalized name) were folded into this row
    pub dupe_count: i32,
    /// which other indexers carried the folded copies
    pub also_on: Vec<String>,
    pub kind: String, // "movie" | "tv" | "other"
    /// false when the release name doesn't actually contain the query terms —
    /// some indexers return filler on weak matches; the UI hides these by default
    pub relevant: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexerOutcome {
    pub id: i32,
    pub name: String,
    pub count: usize,
    pub ok: bool,
    pub timed_out: bool,
    pub elapsed_ms: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub releases: Vec<EnrichedRelease>,
    pub total_before_dedupe: usize,
    pub indexers: Vec<IndexerOutcome>,
}

fn kind_of(r: &ProwlarrRelease) -> String {
    let mut is_movie = false;
    let mut is_tv = false;
    for c in &r.categories {
        if (2000..3000).contains(&c.id) {
            is_movie = true;
        }
        if (5000..6000).contains(&c.id) {
            is_tv = true;
        }
    }
    if is_tv {
        "tv".into()
    } else if is_movie {
        "movie".into()
    } else {
        "other".into()
    }
}

pub(crate) fn score(r: &ProwlarrRelease, p: &ParsedRelease) -> f64 {
    let mut s = 0.0;
    s += match p.resolution.as_deref() {
        Some("2160p") => 34.0,
        Some("1080p") => 30.0,
        Some("720p") => 14.0,
        _ => 0.0,
    };
    s += match p.source.as_deref() {
        Some("Remux") => 16.0,
        Some("BluRay") => 13.0,
        Some("WEB-DL") => 11.0,
        Some("WEBRip") => 8.0,
        Some("HDTV") => 4.0,
        Some("DVD") => 2.0,
        Some("SCR") => -30.0,
        Some("TS") | Some("TC") => -40.0,
        Some("CAM") => -50.0,
        _ => 0.0,
    };
    if p.proper {
        s += 3.0;
    }
    // Swarm health matters, but it must not outrank quality: a heavily-seeded
    // CAM used to beat every real release. Logarithmic only — the difference
    // between 5 and 50 seeders is meaningful, 400 vs 900 is not.
    let seeders = r.seeders.unwrap_or(0).max(0) as f64;
    s += seeders.ln_1p() * 5.0; // ~0-35 across realistic swarm sizes
    if seeders < 3.0 {
        s -= 12.0; // barely-alive swarms are a real cost to the user
    }
    // Suspiciously tiny "movies" are almost always fakes.
    if r.size > 0 && r.size < 80_000_000 {
        s -= 60.0;
    }
    s
}

/// Fold common accented Latin letters to ASCII so "Kızılcık"/"Shōgun" match
/// the unaccented spellings release groups actually use.
pub(crate) fn fold_diacritic(c: char) -> Option<&'static str> {
    Some(match c {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'ç' | 'ć' | 'č' => "c",
        'é' | 'è' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => "e",
        'í' | 'ì' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => "i",
        'ñ' | 'ń' | 'ň' => "n",
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' | 'ō' | 'ő' => "o",
        'ś' | 'š' | 'ş' => "s",
        'ú' | 'ù' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => "u",
        'ý' | 'ÿ' => "y",
        'ž' | 'ź' | 'ż' => "z",
        'ğ' => "g",
        'ł' => "l",
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        'þ' => "th",
        'ð' => "d",
        _ => return None,
    })
}

pub(crate) fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for c in s.chars() {
        if let Some(folded) = fold_diacritic(c.to_lowercase().next().unwrap_or(c)) {
            out.push_str(folded);
            last_space = false;
        } else if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out
}

/// Every word of the query must appear (as a substring) in the release name.
/// Protects against indexers that answer weak matches with unrelated filler.
fn matches_query(query: &str, title: &str) -> bool {
    let title = normalize(title);
    normalize(query)
        .split(' ')
        .filter(|t| !t.is_empty())
        .all(|t| title.contains(t))
}

fn dedupe_key(r: &ProwlarrRelease) -> String {
    if let Some(h) = &r.info_hash {
        if !h.is_empty() {
            return format!("hash:{}", h.to_lowercase());
        }
    }
    let norm: String = r
        .title
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    if norm.len() < 4 {
        // too little signal to fold on — treat the release as unique
        return format!("guid:{}", r.guid.as_deref().unwrap_or(&r.title));
    }
    format!("title:{norm}")
}

#[tauri::command]
pub async fn search(
    state: State<'_, AppState>,
    query: String,
    kind: String,
    indexer_ids: Vec<i32>,
) -> Result<SearchResponse> {
    perform_search(state.inner(), &query, &kind, &indexer_ids).await
}

/// Core search, shared by the UI command and the follow scheduler.
pub async fn perform_search(
    state: &AppState,
    query: &str,
    kind: &str, // "all" | "movies" | "tv"
    indexer_ids: &[i32],
) -> Result<SearchResponse> {
    let cfg = state.config.read().await.clone();
    let client = prowlarr(&state.http, &cfg)?;

    // "all" deliberately has NO category restriction: things like OS images or
    // concerts live outside 2000/5000, and indexer category tagging is messy.
    let categories: Vec<i32> = match kind {
        "movies" => vec![CAT_MOVIES],
        "tv" => vec![CAT_TV],
        _ => vec![],
    };

    // Query every indexer separately and in parallel, each with its own
    // deadline. Prowlarr's aggregate endpoint waits for the slowest indexer
    // (TPB has been seen taking 50s), which made the whole app feel hung.
    const INDEXER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    let targets: Vec<ProwlarrIndexer> = client
        .indexers()
        .await?
        .into_iter()
        .filter(|i| i.enable)
        .filter(|i| indexer_ids.is_empty() || indexer_ids.contains(&i.id))
        .collect();

    let searches = targets.iter().map(|idx| {
        let client = &client;
        let categories = &categories;
        let query = query.trim();
        async move {
            let started = std::time::Instant::now();
            let result =
                tokio::time::timeout(INDEXER_TIMEOUT, client.search(query, categories, &[idx.id], 100))
                    .await;
            let elapsed_ms = started.elapsed().as_millis() as i64;
            match result {
                Ok(Ok(releases)) => (
                    IndexerOutcome {
                        id: idx.id,
                        name: idx.name.clone(),
                        count: releases.len(),
                        ok: true,
                        timed_out: false,
                        elapsed_ms,
                    },
                    releases,
                ),
                Ok(Err(e)) => {
                    eprintln!("[trawler] indexer {} failed: {e}", idx.name);
                    (
                        IndexerOutcome {
                            id: idx.id,
                            name: idx.name.clone(),
                            count: 0,
                            ok: false,
                            timed_out: false,
                            elapsed_ms,
                        },
                        vec![],
                    )
                }
                Err(_) => (
                    IndexerOutcome {
                        id: idx.id,
                        name: idx.name.clone(),
                        count: 0,
                        ok: false,
                        timed_out: true,
                        elapsed_ms,
                    },
                    vec![],
                ),
            }
        }
    });

    let outcomes = futures::future::join_all(searches).await;
    let mut indexer_outcomes = Vec::with_capacity(outcomes.len());
    let mut raw = Vec::new();
    for (outcome, releases) in outcomes {
        indexer_outcomes.push(outcome);
        raw.extend(releases);
    }

    // Every indexer errored outright (not timeouts) → a real failure worth surfacing.
    if !indexer_outcomes.is_empty()
        && indexer_outcomes.iter().all(|o| !o.ok && !o.timed_out)
    {
        return Err(AppError::Other(
            "all indexers failed — is Prowlarr healthy?".into(),
        ));
    }

    let total = raw.len();
    eprintln!("[trawler] search {query:?} kind={kind} -> {total} raw results");

    // Fold duplicates, keeping the copy with the most seeders as the face of the group.
    let mut groups: HashMap<String, EnrichedRelease> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for r in raw {
        let key = dedupe_key(&r);
        let parsed = parse::parse(&r.title);
        let sc = score(&r, &parsed);
        match groups.get_mut(&key) {
            None => {
                order.push(key.clone());
                let kind = kind_of(&r);
                let relevant = matches_query(query, &r.title);
                groups.insert(
                    key,
                    EnrichedRelease {
                        parsed,
                        score: sc,
                        dupe_count: 1,
                        also_on: vec![],
                        kind,
                        relevant,
                        release: r,
                    },
                );
            }
            Some(existing) => {
                existing.dupe_count += 1;
                let name = r.indexer.clone().unwrap_or_default();
                if !name.is_empty() && !existing.also_on.contains(&name) {
                    let keep_name = existing.release.indexer.clone().unwrap_or_default();
                    if name != keep_name {
                        existing.also_on.push(name);
                    }
                }
                let better = r.seeders.unwrap_or(0) > existing.release.seeders.unwrap_or(0);
                if better {
                    let old_indexer = existing.release.indexer.clone().unwrap_or_default();
                    if !old_indexer.is_empty()
                        && !existing.also_on.contains(&old_indexer)
                    {
                        existing.also_on.push(old_indexer);
                    }
                    existing.release = r;
                    existing.parsed = parse::parse(&existing.release.title);
                    existing.score = score(&existing.release, &existing.parsed);
                    existing.relevant = matches_query(query, &existing.release.title);
                    existing
                        .also_on
                        .retain(|n| Some(n) != existing.release.indexer.as_ref());
                }
            }
        }
    }

    let mut releases: Vec<EnrichedRelease> =
        order.into_iter().filter_map(|k| groups.remove(&k)).collect();
    releases.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    Ok(SearchResponse {
        releases,
        total_before_dedupe: total,
        indexers: indexer_outcomes,
    })
}

// ---------- grab ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrabResult {
    pub ok: bool,
    pub detail: String,
}

#[tauri::command]
pub async fn grab(
    state: State<'_, AppState>,
    title: String,
    kind: String, // "movie" | "tv" | "other"
    magnet_url: Option<String>,
    download_url: Option<String>,
) -> Result<GrabResult> {
    let cfg = state.config.read().await.clone();
    let save_path = match kind.as_str() {
        "movie" => cfg.save_path_movies.clone(),
        "tv" => cfg.save_path_tv.clone(),
        _ => String::new(),
    };
    let save_path = if save_path.is_empty() { None } else { Some(save_path) };
    perform_grab(state.inner(), &title, magnet_url, download_url, save_path).await
}

/// Core grab, shared by the UI command and the follow scheduler.
pub async fn perform_grab(
    state: &AppState,
    title: &str,
    magnet_url: Option<String>,
    download_url: Option<String>,
    save_path: Option<String>,
) -> Result<GrabResult> {
    let cfg = state.config.read().await.clone();
    let q = qbit(&state.http, &cfg);
    let save_path = save_path.unwrap_or_default();

    if !cfg.qbit_category.is_empty() {
        // Best effort — a failure here shouldn't block the grab.
        let _ = q.ensure_category(&cfg.qbit_category, "").await;
    }

    // Prefer the magnet; otherwise pull the .torrent through Prowlarr so
    // passkey-bearing URLs never leave this machine's Prowlarr instance.
    let (magnet, torrent_bytes) = match (&magnet_url, &download_url) {
        (Some(m), _) if !m.is_empty() => (Some(m.clone()), None),
        (_, Some(d)) if !d.is_empty() => {
            let client = prowlarr(&state.http, &cfg)?;
            let (bytes, maybe_magnet) = client.fetch_torrent(d).await?;
            match maybe_magnet {
                Some(m) => (Some(m), None),
                None => (None, Some(bytes)),
            }
        }
        _ => return Err(AppError::NoDownloadSource),
    };

    let ratio_limit = match cfg.seed_policy.as_str() {
        "none" => Some(0.0),
        "ratio" => Some(cfg.seed_ratio.max(0.0)),
        _ => None, // qBittorrent's own settings
    };
    q.add(AddTorrent {
        magnet,
        torrent_bytes,
        torrent_name: title,
        save_path: if save_path.is_empty() { None } else { Some(save_path) },
        category: if cfg.qbit_category.is_empty() { None } else { Some(cfg.qbit_category.clone()) },
        paused: cfg.add_paused,
        ratio_limit,
    })
    .await?;

    Ok(GrabResult { ok: true, detail: format!("Sent to qBittorrent: {title}") })
}

// ---------- downloads ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadsView {
    pub torrents: Vec<QbitTorrent>,
    pub transfer: Option<TransferInfo>,
}

#[tauri::command]
pub async fn downloads(state: State<'_, AppState>, all: bool) -> Result<DownloadsView> {
    let cfg = state.config.read().await.clone();
    let q = qbit(&state.http, &cfg);
    let category = if all || cfg.qbit_category.is_empty() {
        None
    } else {
        Some(cfg.qbit_category.as_str())
    };
    let torrents = q.list(category).await?;
    let transfer = q.transfer_info().await.ok();
    Ok(DownloadsView { torrents, transfer })
}

#[tauri::command]
pub async fn torrent_action(
    state: State<'_, AppState>,
    action: String,
    hash: String,
) -> Result<()> {
    let cfg = state.config.read().await.clone();
    qbit(&state.http, &cfg).torrent_action(&action, &hash).await
}

#[cfg(test)]
mod tests {
    use super::matches_query;

    #[test]
    fn query_matching() {
        assert!(matches_query("ubuntu", "ubuntu-24.04.1-desktop-amd64.iso"));
        assert!(matches_query("the expanse s02", "The.Expanse.S02E05.1080p.WEB-DL-NTb"));
        assert!(matches_query("dune part two", "Dune Part Two (2024) 1080p WEBRip"));
        assert!(matches_query("Dune: Part Two", "Dune.Part.Two.2024.2160p.REMUX"));
        assert!(!matches_query("ubuntu", "Big Buck Bunny 1080p"));
        assert!(!matches_query("the expanse s03", "The.Expanse.S02E05.1080p"));
        assert!(matches_query("", "anything at all"));
    }
}

// ---------- followed shows ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TvmazeResult {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub premiered: Option<String>,
    pub ended: Option<String>,
    pub network: Option<String>,
    pub poster: Option<String>,
    pub genres: Vec<String>,
    pub summary: Option<String>,
    pub followed: bool,
}

#[tauri::command]
pub async fn search_tvmaze(state: State<'_, AppState>, query: String) -> Result<Vec<TvmazeResult>> {
    let shows = crate::tvmaze::search_shows(&state.http, query.trim()).await?;
    let followed_ids: Vec<i64> = {
        let conn = state.db.lock().await;
        crate::db::list_shows(&conn)?.iter().map(|s| s.tvmaze_id).collect()
    };
    Ok(shows
        .into_iter()
        .map(|s| TvmazeResult {
            followed: followed_ids.contains(&s.id),
            poster: s.poster(),
            network: s.network_name(),
            id: s.id,
            name: s.name,
            status: s.status,
            premiered: s.premiered,
            ended: s.ended,
            genres: s.genres,
            summary: s.summary,
        })
        .collect())
}

#[tauri::command]
pub async fn follow_show(
    state: State<'_, AppState>,
    tvmaze_id: i64,
    backfill: bool,
    seasons: Option<Vec<i64>>,
) -> Result<crate::db::ShowRow> {
    crate::follows::follow(&state, tvmaze_id, backfill, seasons).await
}

#[tauri::command]
pub async fn unfollow_show(state: State<'_, AppState>, tvmaze_id: i64) -> Result<()> {
    let conn = state.db.lock().await;
    conn.execute("DELETE FROM shows WHERE tvmaze_id = ?1", [tvmaze_id])
        .map_err(crate::db::db_err)?;
    Ok(())
}

#[tauri::command]
pub async fn get_shows(state: State<'_, AppState>) -> Result<Vec<crate::db::ShowRow>> {
    let conn = state.db.lock().await;
    crate::db::list_shows(&conn)
}

#[tauri::command]
pub async fn get_show_episodes(
    state: State<'_, AppState>,
    tvmaze_id: i64,
) -> Result<Vec<crate::db::EpisodeRow>> {
    let conn = state.db.lock().await;
    crate::db::list_episodes(&conn, tvmaze_id)
}

#[tauri::command]
pub async fn refresh_show(state: State<'_, AppState>, tvmaze_id: i64) -> Result<()> {
    crate::follows::refresh_show(&state, tvmaze_id).await
}

/// Manual state changes from the UI: wanted | ignored | downloaded.
/// Marking `grabbed` happens through the grab flow, not here.
#[tauri::command]
pub async fn set_episode_state(
    state: State<'_, AppState>,
    tvmaze_ep_id: i64,
    new_state: String,
    grabbed_title: Option<String>,
) -> Result<()> {
    if !["wanted", "ignored", "downloaded", "grabbed"].contains(&new_state.as_str()) {
        return Err(AppError::Other(format!("invalid episode state {new_state}")));
    }
    let conn = state.db.lock().await;
    conn.execute(
        "UPDATE episodes SET state = ?1,
                grabbed_title = COALESCE(?2, grabbed_title),
                grabbed_at = CASE WHEN ?1 = 'grabbed' THEN ?3 ELSE grabbed_at END
         WHERE tvmaze_ep_id = ?4",
        rusqlite::params![new_state, grabbed_title, crate::db::now(), tvmaze_ep_id],
    )
    .map_err(crate::db::db_err)?;
    Ok(())
}

#[tauri::command]
pub async fn set_show_options(
    state: State<'_, AppState>,
    tvmaze_id: i64,
    quality_json: Option<String>,
    save_path_override: Option<String>,
) -> Result<()> {
    let conn = state.db.lock().await;
    conn.execute(
        "UPDATE shows SET quality_json = ?1, save_path_override = ?2 WHERE tvmaze_id = ?3",
        rusqlite::params![quality_json, save_path_override, tvmaze_id],
    )
    .map_err(crate::db::db_err)?;
    Ok(())
}

#[tauri::command]
pub async fn get_activity(state: State<'_, AppState>) -> Result<Vec<crate::db::ActivityRow>> {
    let conn = state.db.lock().await;
    crate::db::list_activity(&conn, 100)
}

/// Dry run: what would the next scheduler cycle grab for this show?
#[tauri::command]
pub async fn preview_show_grabs(
    state: State<'_, AppState>,
    tvmaze_id: i64,
) -> Result<Vec<crate::scheduler::PlannedGrab>> {
    let show = {
        let conn = state.db.lock().await;
        crate::db::list_shows(&conn)?
            .into_iter()
            .find(|s| s.tvmaze_id == tvmaze_id)
            .ok_or_else(|| AppError::Other("show not followed".into()))?
    };
    Ok(crate::scheduler::plan_for_show(state.inner(), &show, true).await?.plans)
}

// ---------- indexer manager ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexerDef {
    pub name: String,
    pub description: String,
    pub privacy: String,
    pub language: String,
}

/// Installable public definitions from Prowlarr's catalog, slimmed for the picker.
#[tauri::command]
pub async fn indexer_defs(state: State<'_, AppState>) -> Result<Vec<IndexerDef>> {
    let cfg = state.config.read().await.clone();
    let defs = prowlarr(&state.http, &cfg)?.schema().await?;
    let mut out: Vec<IndexerDef> = defs
        .iter()
        .filter(|d| d.get("privacy").and_then(|v| v.as_str()) == Some("public"))
        .filter(|d| d.get("protocol").and_then(|v| v.as_str()) == Some("torrent"))
        .map(|d| IndexerDef {
            name: d.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
            description: d
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(90)
                .collect(),
            privacy: "public".into(),
            language: d.get("language").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        })
        .collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// Install an indexer from its catalog definition (defaults untouched).
#[tauri::command]
pub async fn add_indexer(state: State<'_, AppState>, name: String) -> Result<String> {
    let cfg = state.config.read().await.clone();
    let client = prowlarr(&state.http, &cfg)?;
    let defs = client.schema().await?;
    let mut def = defs
        .into_iter()
        .find(|d| d.get("name").and_then(|v| v.as_str()) == Some(name.as_str()))
        .ok_or_else(|| AppError::Other(format!("no definition named {name}")))?;
    def["enable"] = serde_json::Value::Bool(true);
    def["appProfileId"] = serde_json::Value::from(1);
    let added = match client.add_indexer_raw(&def).await {
        Ok(a) => a,
        Err(e) if format!("{e}").to_lowercase().contains("cloudflare") => {
            // Cloudflare wall — retry through FlareSolverr ONLY when the user
            // actually opted in (managed install present + proxy registered);
            // adding an indexer must never mutate Prowlarr's global config
            let opted_in = crate::setup::managed_flaresolverr_exe().exists()
                && crate::setup::flaresolverr_running(&state).await
                && client.flaresolverr_proxy_exists().await.unwrap_or(false);
            if !opted_in {
                return Err(e);
            }
            let tag_id = client.ensure_tag("flaresolverr").await?;
            def["tags"] = serde_json::json!([tag_id]);
            client.add_indexer_raw(&def).await?
        }
        Err(e) => return Err(e),
    };
    Ok(added
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(&name)
        .to_string())
}

#[tauri::command]
pub async fn toggle_indexer(state: State<'_, AppState>, id: i32, enable: bool) -> Result<()> {
    let cfg = state.config.read().await.clone();
    let client = prowlarr(&state.http, &cfg)?;
    let mut def = client.get_indexer_raw(id).await?;
    def["enable"] = serde_json::Value::Bool(enable);
    client.update_indexer_raw(id, &def).await
}

#[tauri::command]
pub async fn remove_indexer(state: State<'_, AppState>, id: i32) -> Result<()> {
    let cfg = state.config.read().await.clone();
    prowlarr(&state.http, &cfg)?.delete_indexer(id).await
}

// ---------- autostart ----------

#[tauri::command]
pub async fn get_autostart(app: tauri::AppHandle) -> Result<bool> {
    use tauri_plugin_autostart::ManagerExt;
    Ok(app.autolaunch().is_enabled().unwrap_or(false))
}

#[tauri::command]
pub async fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<()> {
    use tauri_plugin_autostart::ManagerExt;
    let launcher = app.autolaunch();
    let r = if enabled { launcher.enable() } else { launcher.disable() };
    r.map_err(|e| AppError::Other(format!("autostart: {e}")))
}

/// Send a test message to every configured notification channel.
#[tauri::command]
pub async fn notify_test(state: State<'_, AppState>) -> Result<crate::notify::TestOutcome> {
    let cfg = state.config.read().await.clone();
    Ok(crate::notify::send_test(&state.http, &cfg).await)
}

/// Run one upgrade-scout pass immediately (fire and forget; results land in
/// the activity feed and the proposal inbox).
#[tauri::command]
pub async fn upgrade_scan_now(app: tauri::AppHandle) -> Result<()> {
    tauri::async_runtime::spawn(async move {
        match crate::upgrade::scan_now(&app).await {
            Ok(n) => eprintln!("[trawler] manual upgrade scan done: {n} proposal(s)"),
            Err(e) => eprintln!("[trawler] manual upgrade scan failed: {e}"),
        }
    });
    Ok(())
}

/// Kick a scheduler cycle immediately (fire and forget).
#[tauri::command]
pub async fn run_scheduler_now(app: tauri::AppHandle) -> Result<()> {
    tauri::async_runtime::spawn(async move {
        match crate::scheduler::run_cycle(&app).await {
            Ok(n) => eprintln!("[trawler] manual cycle done: {n} grabs"),
            Err(e) => eprintln!("[trawler] manual cycle failed: {e}"),
        }
    });
    Ok(())
}
