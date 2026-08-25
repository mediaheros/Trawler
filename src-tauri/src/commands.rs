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
    // the Bitport token never travels to the webview: Settings round-trips
    // the whole config as a draft, and a draft must be able to neither wipe
    // nor read the secret
    let mut cfg = state.config.read().await.clone();
    cfg.bitport_token.clear();
    Ok(cfg)
}

#[tauri::command]
pub async fn set_config(state: State<'_, AppState>, config: Config) -> Result<Config> {
    // only bitport_connect / bitport_disconnect write the token — whatever
    // the frontend sends in that field is a stale blank, not intent.
    // Read-modify-write happens UNDER the write lock: a Save racing a
    // connect must not resurrect the old token or drop the new one.
    let mut config = config;
    {
        let mut guard = state.config.write().await;
        config.bitport_token = guard.bitport_token.clone();
        config::save(&config)?;
        *guard = config.clone();
    }
    config.bitport_token.clear();
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
                    crate::applog::warn("prowlarr", format!("indexer {} failed: {e}", idx.name));
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
    // All attempted indexers timed out with nothing to show → an indexer
    // health problem, not "no results" — surfacing it as an empty success
    // sends the user hunting for a query that was never the problem.
    if !indexer_outcomes.is_empty()
        && raw.is_empty()
        && indexer_outcomes.iter().all(|o| o.timed_out)
    {
        return Err(AppError::Other(
            "every indexer timed out — check their health in Settings → Indexers".into(),
        ));
    }

    let total = raw.len();

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

    {
        let failed: Vec<String> = indexer_outcomes
            .iter()
            .filter(|o| !o.ok)
            .map(|o| format!("{}{}", o.name, if o.timed_out { " (timeout)" } else { " (error)" }))
            .collect();
        crate::applog::info(
            "prowlarr",
            format!(
                "search \"{}\" -> {} releases from {} indexers{}",
                query,
                releases.len(),
                indexer_outcomes.iter().filter(|o| o.ok).count(),
                if failed.is_empty() { String::new() } else { format!(" · failed: {}", failed.join(", ")) }
            ),
        );
    }
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
#[allow(clippy::too_many_arguments)] // Tauri exposes these as named invoke parameters.
pub async fn grab(
    state: State<'_, AppState>,
    title: String,
    kind: String, // "movie" | "tv" | "other"
    magnet_url: Option<String>,
    download_url: Option<String>,
    info_hash: Option<String>,
    size: Option<i64>,
    ep_ids: Option<Vec<i64>>,
) -> Result<GrabResult> {
    let save_path = {
        let cfg = state.config.read().await.clone();
        let save_path = match kind.as_str() {
            "movie" => cfg.save_path_movies.clone(),
            "tv" => cfg.save_path_tv.clone(),
            _ => String::new(),
        };
        if save_path.is_empty() { None } else { Some(save_path) }
    };
    // Manual grabs go through the same claim-then-dispatch as every other
    // path: the ledger row is written even if this caller goes away, and a
    // linked episode is stamped grabbed server-side.
    let outcome = crate::grab::dispatch(
        state.inner(),
        crate::grab::GrabOrder {
            title: title.clone(),
            magnet_url,
            download_url,
            save_path,
            info_hash,
            size: size.unwrap_or(0),
        },
        None,
        ep_ids.unwrap_or_default(),
    )
    .await?;
    let detail = match outcome {
        crate::grab::GrabOutcome::Grabbed { backend } => match backend {
            "bitport" => format!("Sent to your Bitport cloud: {title}"),
            _ => format!("Sent to qBittorrent: {title}"),
        },
        crate::grab::GrabOutcome::AlreadyClaimed => {
            "Already being grabbed right now — one moment".to_string()
        }
        crate::grab::GrabOutcome::AlreadyHad => {
            "Already in your library — recorded earlier".to_string()
        }
    };
    Ok(GrabResult { ok: true, detail })
}

/// The qBittorrent add itself, factored out so `grab::dispatch` can run it
/// on a task that survives caller cancellation. No ledger writes here —
/// recording belongs to the dispatcher, next to the add.
pub async fn perform_grab_core(
    http: &reqwest::Client,
    cfg: &Config,
    order: &crate::grab::GrabOrder,
    content_key: &str,
) -> Result<()> {
    let q = qbit(http, cfg);
    let save_path = order.save_path.clone().unwrap_or_default();

    if !cfg.qbit_category.is_empty() {
        // Best effort — a failure here shouldn't block the grab.
        let _ = q.ensure_category(&cfg.qbit_category, "").await;
    }

    // Prefer the magnet; otherwise pull the .torrent through Prowlarr so
    // passkey-bearing URLs never leave this machine's Prowlarr instance.
    let (magnet, torrent_bytes) = match (&order.magnet_url, &order.download_url) {
        (Some(m), _) if !m.is_empty() => (Some(m.clone()), None),
        (_, Some(d)) if !d.is_empty() => {
            let client = prowlarr(http, cfg)?;
            let (bytes, maybe_magnet) = client.fetch_torrent(d).await?;
            match maybe_magnet {
                Some(m) => (Some(m), None),
                None => (None, Some(bytes)),
            }
        }
        _ => return Err(AppError::NoDownloadSource),
    };

    if order.info_hash.is_none() {
        let resolved_hash = crate::scheduler::magnet_hash(magnet.as_deref())
            .or_else(|| torrent_bytes.as_deref().and_then(crate::qbit::torrent_info_hash));
        if let Some(info_hash) = resolved_hash {
            let conn = crate::db::open_existing()?;
            crate::db::ledger_set_dispatch_info_hash(&conn, content_key, &info_hash)?;
        }
    }

    let ratio_limit = match cfg.seed_policy.as_str() {
        "none" => Some(0.0),
        "ratio" => Some(cfg.seed_ratio.max(0.0)),
        _ => None, // qBittorrent's own settings
    };
    q.add(AddTorrent {
        magnet,
        torrent_bytes,
        torrent_name: &order.title,
        save_path: if save_path.is_empty() { None } else { Some(save_path) },
        category: if cfg.qbit_category.is_empty() { None } else { Some(cfg.qbit_category.clone()) },
        paused: cfg.add_paused,
        ratio_limit,
    })
    .await?;

    Ok(())
}

// ---------- downloads ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadsView {
    pub torrents: Vec<QbitTorrent>,
    pub transfer: Option<TransferInfo>,
    pub cloud: Vec<crate::bitport::BitportTransfer>,
    pub qbit_error: Option<String>,
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
    // qBittorrent being down must not take the cloud section with it — the
    // backends are independent; surface the local error inline instead
    let (torrents, qbit_error) = match q.list(category).await {
        Ok(t) => (t, None),
        Err(e) => (vec![], Some(e.to_string())),
    };
    let transfer = if qbit_error.is_none() { q.transfer_info().await.ok() } else { None };
    // cloud transfers ride along whenever an account is connected — additive,
    // and a Bitport hiccup must never blank the local list
    let cloud = if cfg.bitport_token.is_empty() {
        vec![]
    } else {
        let bp = crate::bitport::BitportClient { http: &state.http, token: cfg.bitport_token.clone() };
        // short leash: this rides the 2s Downloads poll — a Bitport hiccup
        // must never stall the local list behind a 20s network timeout
        match tokio::time::timeout(std::time::Duration::from_secs(4), bp.transfers()).await {
            Ok(Ok(t)) => t,
            _ => vec![],
        }
    };
    Ok(DownloadsView { torrents, transfer, cloud, qbit_error })
}

#[tauri::command]
pub async fn torrent_action(
    state: State<'_, AppState>,
    action: String,
    hash: String,
) -> Result<()> {
    let cfg = state.config.read().await.clone();
    let client = qbit(&state.http, &cfg);
    // deleting a grab must also RELEASE it in Trawler: otherwise the ledger's
    // anti-double-grab memory blocks the user's deliberate re-download, and
    // episodes sit "grabbed" forever pointing at a torrent that's gone
    let removed_name = if action.starts_with("delete") {
        client
            .list(None)
            .await
            .ok()
            .and_then(|ts| ts.into_iter().find(|t| t.hash.eq_ignore_ascii_case(&hash)))
            .map(|t| t.name)
    } else {
        None
    };
    client.torrent_action(&action, &hash).await?;
    if let Some(name) = removed_name {
        let conn = state.db.lock().await;
        let norm_name = normalize(&name);
        let h = hash.to_lowercase();
        // retire ledger rows for this torrent — hash first, title fallback
        let rows: Vec<(i64, String, Option<String>, Option<String>)> = conn
            .prepare("SELECT id, title, info_hash, ep_ids FROM grab_ledger WHERE state IN ('grabbed','completed') AND backend = 'qbittorrent'")
            .ok()
            .map(|mut stmt| {
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                    .map(|it| it.flatten().collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let mut freed: Vec<String> = vec![];
        for (id, title, info_hash, ep_ids_raw) in rows {
            let hash_match = info_hash.map(|x| x.eq_ignore_ascii_case(&h)).unwrap_or(false);
            if hash_match || normalize(&title) == norm_name {
                let _ = conn.execute("UPDATE grab_ledger SET state = 'removed' WHERE id = ?1", [id]);
                crate::db::set_episodes_state_by_ids(
                    &conn,
                    &crate::db::parse_ep_ids(ep_ids_raw.as_deref()),
                    "wanted",
                    None,
                );
                freed.push(title);
            }
        }
        // episodes still pointing at this grab go back to wanted
        let eps: Vec<(i64, String)> = conn
            .prepare("SELECT tvmaze_ep_id, grabbed_title FROM episodes WHERE state = 'grabbed' AND grabbed_title IS NOT NULL")
            .ok()
            .map(|mut stmt| {
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map(|it| it.flatten().collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let mut ep_count = 0;
        for (ep_id, gt) in eps {
            let gt_matches = normalize(&gt) == norm_name
                || freed.iter().any(|f| normalize(f) == normalize(&gt));
            if gt_matches {
                let _ = conn.execute(
                    "UPDATE episodes SET state = 'wanted', grabbed_title = NULL, grabbed_at = NULL,
                            last_searched_at = 0
                     WHERE tvmaze_ep_id = ?1",
                    [ep_id],
                );
                ep_count += 1;
            }
        }
        if !freed.is_empty() || ep_count > 0 {
            crate::db::log_activity(
                &conn,
                "system",
                None,
                &format!(
                    "Removed {} — released {} episode(s); Trawler can grab them again",
                    name.chars().take(60).collect::<String>(),
                    ep_count
                ),
            );
        }
    }
    Ok(())
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
    // an empty selection is a one-way trap once persisted ("[]" gates every
    // episode to ignored and COALESCE can't clear it) — treat it as "all"
    let seasons = seasons.filter(|s| !s.is_empty());
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
    out.sort_by_key(|a| a.name.to_lowercase());
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
            // a registered proxy is deliberate opt-in (only setup_flaresolverr
            // or the user's own Prowlarr config creates one) — no exe check,
            // which also covers the macOS Docker route
            let opted_in = crate::setup::flaresolverr_running(&state).await
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

// ---------- bitport (cloud backend) ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BitportStatus {
    pub connected: bool,
    pub quota: Option<crate::bitport::BitportQuota>,
}

#[tauri::command]
pub async fn bitport_authorize_url() -> Result<String> {
    Ok(crate::bitport::authorize_url(None))
}

/// The whole connect handshake behind one button: claim the callback port,
/// open the browser, catch the redirect, exchange the code, prove the token.
/// Nothing to copy, nothing to paste.
#[tauri::command]
pub async fn bitport_connect_flow(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<BitportStatus> {
    use tauri_plugin_opener::OpenerExt;
    // bind first: a blocked port must fail now, with a reason, not after a
    // five-minute wait on a redirect that could never arrive
    let listener = crate::bitport::bind_callback().await?;
    let oauth_state = crate::bitport::new_oauth_state()?;
    let url = crate::bitport::authorize_url(Some(&oauth_state));
    crate::applog::info("bitport", "waiting for browser approval on 127.0.0.1:8788");
    app.opener()
        .open_url(url.clone(), None::<&str>)
        .map_err(|e| AppError::Other(format!("could not open your browser ({e}) — visit {url} manually")))?;
    let code = crate::bitport::await_code(
        listener,
        std::time::Duration::from_secs(300),
        &oauth_state,
    )
    .await?;
    let token = crate::bitport::exchange_code(&state.http, &code).await?;
    bitport_store_and_probe(&state, token).await
}

/// A minted token is stored unless Bitport itself rejects it (401): the
/// authorization code is single-use, so discarding a good token on a
/// transient probe failure would burn the user's whole approval.
async fn bitport_store_and_probe(state: &AppState, token: String) -> Result<BitportStatus> {
    let bp = crate::bitport::BitportClient { http: &state.http, token: token.clone() };
    match bp.me().await {
        Ok(quota) => {
            let mut cfg = state.config.write().await;
            cfg.bitport_token = token;
            crate::config::save(&cfg)?;
            crate::applog::info(
                "bitport",
                format!("connected — plan {}, {:.0} GB free", quota.plan_name, quota.disk_available as f64 / 1e9),
            );
            Ok(BitportStatus { connected: true, quota: Some(quota) })
        }
        Err(e) if e.to_string().contains("rejected the token") => Err(e),
        Err(e) => {
            let mut cfg = state.config.write().await;
            cfg.bitport_token = token;
            crate::config::save(&cfg)?;
            crate::applog::warn(
                "bitport",
                format!("connected, but the account probe failed ({e}) — quota will appear once Bitport answers"),
            );
            Ok(BitportStatus { connected: true, quota: None })
        }
    }
}

/// Exchange the pasted authorization code and persist the token.
#[tauri::command]
pub async fn bitport_connect(state: State<'_, AppState>, code: String) -> Result<BitportStatus> {
    let code = crate::bitport::extract_code(&code);
    if code.is_empty() {
        return Err(AppError::Other("paste the code Bitport showed you".into()));
    }
    let token = crate::bitport::exchange_code(&state.http, &code).await?;
    bitport_store_and_probe(&state, token).await
}

#[tauri::command]
pub async fn bitport_status(state: State<'_, AppState>) -> Result<BitportStatus> {
    let cfg = state.config.read().await.clone();
    if cfg.bitport_token.is_empty() {
        return Ok(BitportStatus { connected: false, quota: None });
    }
    let bp = crate::bitport::BitportClient { http: &state.http, token: cfg.bitport_token.clone() };
    match bp.me().await {
        Ok(q) => Ok(BitportStatus { connected: true, quota: Some(q) }),
        // connected-but-unreachable still counts as connected; quota just absent
        Err(_) => Ok(BitportStatus { connected: true, quota: None }),
    }
}

#[tauri::command]
pub async fn bitport_disconnect(state: State<'_, AppState>) -> Result<()> {
    let mut cfg = state.config.write().await;
    cfg.bitport_token.clear();
    if cfg.download_backend == "bitport" {
        cfg.download_backend = "qbittorrent".into();
    }
    crate::config::save(&cfg)?;
    Ok(())
}

#[tauri::command]
pub async fn bitport_delete(state: State<'_, AppState>, token: String) -> Result<()> {
    let cfg = state.config.read().await.clone();
    let bp = crate::bitport::BitportClient { http: &state.http, token: cfg.bitport_token.clone() };
    // capture the transfer's identity before it disappears — removing a cloud
    // grab must RELEASE it in Trawler, exactly like deleting a local torrent:
    // the ledger's anti-double-grab memory must not outlive the thing it
    // points at, or a deliberate re-download stays blocked forever
    let victim = bp.transfers().await.ok().and_then(|ts| ts.into_iter().find(|t| t.token == token));
    bp.delete_transfer(&token).await?;
    let vic_hash = victim.as_ref().and_then(crate::bitport::transfer_hash);
    let vic_norm = victim.as_ref().map(|t| normalize(&t.name));
    let conn = state.db.lock().await;
    type BitportLedgerRow = (i64, String, Option<String>, Option<String>, Option<String>);
    let rows: Vec<BitportLedgerRow> = conn
        .prepare("SELECT id, title, info_hash, ep_ids, bp_token FROM grab_ledger WHERE backend = 'bitport' AND state IN ('grabbed','completed')")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .map(|it| it.flatten().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let mut freed = 0usize;
    let mut freed_name = String::new();
    for (id, title, info_hash, ep_ids_raw, bp_token) in rows {
        let tok_match = bp_token.as_deref() == Some(token.as_str());
        let hash_match = match (&info_hash, &vic_hash) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        };
        let norm_match = vic_norm.as_ref().map(|n| &normalize(&title) == n).unwrap_or(false);
        if tok_match || hash_match || norm_match {
            let _ = conn.execute("UPDATE grab_ledger SET state = 'removed' WHERE id = ?1", [id]);
            crate::db::set_episodes_state_by_ids(
                &conn,
                &crate::db::parse_ep_ids(ep_ids_raw.as_deref()),
                "wanted",
                None,
            );
            freed += 1;
            freed_name = title;
        }
    }
    if freed > 0 {
        crate::db::log_activity(
            &conn,
            "system",
            None,
            &format!(
                "Removed from your cloud: {} — released; Trawler can grab it again",
                freed_name.chars().take(60).collect::<String>()
            ),
        );
    }
    Ok(())
}

// ---------- log console ----------

#[tauri::command]
pub async fn logs_recent() -> Result<Vec<crate::applog::LogEntry>> {
    Ok(crate::applog::recent())
}

/// One paste = a full diagnosis: app + OS context, qBittorrent's own view of
/// the network, and the buffered log — plus the tail of qBt's log file, where
/// today's port-reservation smoking gun lived.
#[tauri::command]
pub async fn logs_support_bundle(state: State<'_, AppState>) -> Result<String> {
    let cfg = state.config.read().await.clone();
    let mut out = String::new();
    out.push_str(&format!(
        "Trawler {} · {} {}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    let q = qbit(&state.http, &cfg);
    match q.sync_maindata().await {
        Ok(v) => {
            let s = v.get("server_state").cloned().unwrap_or_default();
            out.push_str(&format!(
                "qBittorrent: connection={} dht_nodes={} dl_speed={}\n",
                s.get("connection_status").and_then(|x| x.as_str()).unwrap_or("?"),
                s.get("dht_nodes").and_then(|x| x.as_i64()).unwrap_or(-1),
                s.get("dl_info_speed").and_then(|x| x.as_i64()).unwrap_or(-1),
            ));
        }
        Err(e) => out.push_str(&format!("qBittorrent: unreachable ({e})\n")),
    }
    // the single most useful datum from the saga that motivated all this:
    // the listen port, and whether Windows has quietly reserved it
    if let Ok(p) = q.preferences().await {
        let port = p.get("listen_port").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
        let reserved = tokio::task::spawn_blocking(crate::setup::excluded_port_ranges)
            .await
            .unwrap_or_default();
        let blocked = reserved.iter().any(|(a, b)| port >= *a && port <= *b);
        out.push_str(&format!(
            "listen_port: {port}{}\n",
            if blocked { " ← INSIDE a Windows-reserved range (this is your problem)" } else { "" }
        ));
    }
    let fmt_ts = |ts: i64| {
        chrono::DateTime::from_timestamp(ts, 0)
            .map(|d| d.format("%H:%M:%S").to_string())
            .unwrap_or_else(|| ts.to_string())
    };
    let all = crate::applog::recent();
    let start = all.len().saturating_sub(400);
    out.push_str("\n--- Trawler log ---\n");
    if start > 0 {
        out.push_str(&format!("… {start} earlier entries trimmed …\n"));
    }
    for e in &all[start..] {
        out.push_str(&format!("{} [{}] {}: {}\n", fmt_ts(e.ts), e.level, e.area, e.message));
    }
    if let Some(tail) = crate::setup::qbt_log_tail(60) {
        // qBt logs tracker announce URLs — passkeys ride in those. Scrub.
        out.push_str("\n--- qBittorrent log (last 60 lines, scrubbed) ---\n");
        out.push_str(&crate::applog::scrub(&tail));
    }
    Ok(out)
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
            Ok(n) => crate::applog::info("app",format!("manual upgrade scan done: {n} proposal(s)")),
            Err(e) => crate::applog::warn("app",format!("manual upgrade scan failed: {e}")),
        }
    });
    Ok(())
}

/// Kick a scheduler cycle immediately (fire and forget).
#[tauri::command]
pub async fn run_scheduler_now(app: tauri::AppHandle) -> Result<()> {
    tauri::async_runtime::spawn(async move {
        match crate::scheduler::run_cycle(&app).await {
            Ok(n) => crate::applog::info("app",format!("manual cycle done: {n} grabs")),
            Err(e) => crate::applog::warn("app",format!("manual cycle failed: {e}")),
        }
    });
    Ok(())
}
