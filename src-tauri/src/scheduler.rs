//! The follow scheduler: plans and executes grabs for followed shows.

use chrono::Utc;
use serde::Serialize;
use tauri::Manager;

use crate::commands::{normalize, perform_search, EnrichedRelease};
use crate::config::QualityProfile;
use crate::db::{self, EpisodeRow, ShowRow};
use crate::error::Result;
use crate::follows::{codec_boost, episode_query, profile_allows, refresh_show, season_query};
use crate::AppState;

/// qBittorrent reports a finished torrent as exactly 1.0. Anything short of
/// that is still downloading: 0.999 of a 20 GB pack is 20 MB missing, and a
/// torrent wedged at 99.9% on a dead piece must not read as complete.
fn is_complete_progress(progress: f64) -> bool {
    progress >= 1.0
}

/// Only a qBittorrent-shaped identity (hex v1 or v2 infohash) may override
/// title matching; a ledger row carrying anything else falls back to the name.
fn qbt_comparable_hash(hash: &str) -> bool {
    matches!(hash.len(), 40 | 64) && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Releases the ledger retired as dead swarms. The planner must skip them:
/// once `reopen_dead_grabs` hands the episodes back, the very same corpse is
/// usually still the top-ranked search result (indexer seeder counts are
/// stale), and re-adding it makes qBittorrent reject a duplicate every cycle.
pub struct RetiredReleases {
    titles: std::collections::HashSet<String>,
    hashes: std::collections::HashSet<String>,
}

impl RetiredReleases {
    pub fn contains(&self, r: &EnrichedRelease) -> bool {
        let by_hash = r
            .release
            .info_hash
            .as_deref()
            .map(|h| h.to_ascii_lowercase())
            .or_else(|| magnet_hash(r.release.magnet_url.as_deref()))
            .is_some_and(|h| self.hashes.contains(&h));
        by_hash || self.titles.contains(&normalize(&r.release.title))
    }
}

fn retired_releases(conn: &rusqlite::Connection) -> RetiredReleases {
    let mut out = RetiredReleases { titles: Default::default(), hashes: Default::default() };
    if let Ok(mut stmt) = conn.prepare(
        "SELECT title, info_hash FROM grab_ledger WHERE state = 'stalled' AND backend = 'qbittorrent'",
    ) {
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
            .map(|it| it.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        for (title, hash) in rows {
            out.titles.insert(normalize(&title));
            if let Some(h) = hash.filter(|h| qbt_comparable_hash(h)) {
                out.hashes.insert(h.to_ascii_lowercase());
            }
        }
    }
    out
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedGrab {
    pub show_id: i64,
    pub show_name: String,
    pub season: i64,
    /// episode numbers this grab covers
    pub episodes: Vec<i64>,
    /// tvmaze episode ids this grab covers
    pub ep_ids: Vec<i64>,
    pub title: String,
    pub indexer: Option<String>,
    pub size: i64,
    pub seeders: Option<i32>,
    pub is_pack: bool,
    pub magnet_url: Option<String>,
    pub download_url: Option<String>,
}

fn aired(e: &EpisodeRow) -> bool {
    e.airstamp
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t <= Utc::now())
        .unwrap_or(false)
}

fn aired_within_days(e: &EpisodeRow, days: f64) -> bool {
    e.airstamp
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_hours() as f64 <= days * 24.0)
        .unwrap_or(false)
}

/// Pick the best allowed release for one episode (or one season pack).
/// Pure over already-enriched search results — unit-testable.
pub fn pick_candidate<'a>(
    releases: &'a [EnrichedRelease],
    profile: &QualityProfile,
    season: i64,
    episode: Option<i64>, // None = looking for a season pack
    episodes_covered: i64,
) -> Option<&'a EnrichedRelease> {
    releases
        .iter()
        .filter(|r| r.relevant)
        .filter(|r| r.release.seeders.map(|s| s > 0).unwrap_or(true))
        .filter(|r| r.parsed.season.map(i64::from) == Some(season))
        .filter(|r| match episode {
            Some(e) => r.parsed.episode.map(i64::from) == Some(e) && !r.parsed.season_pack,
            None => r.parsed.season_pack,
        })
        .filter(|r| profile_allows(profile, &r.parsed, r.release.size, episodes_covered))
        .max_by(|a, b| {
            let sa = a.score + codec_boost(profile, &a.parsed);
            let sb = b.score + codec_boost(profile, &b.parsed);
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        })
}

fn profile_for(show: &ShowRow, default: &QualityProfile) -> QualityProfile {
    show.quality_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_else(|| default.clone())
}

/// What one planning pass produced: the grabs to make, and which episodes
/// were ACTUALLY searched with at least one indexer answering — only those
/// may be stamped, or the 12h back-catalog throttle starves episodes that
/// were never looked for.
pub struct PlanOutcome {
    pub plans: Vec<PlannedGrab>,
    pub searched_ep_ids: Vec<i64>,
}

/// Decide what to grab for one show. Read-only: no grabs, no stamps.
pub async fn plan_for_show(
    state: &AppState,
    show: &ShowRow,
    ignore_throttle: bool,
) -> Result<PlanOutcome> {
    let cfg = state.config.read().await.clone();
    let profile = profile_for(show, &cfg.default_quality);
    let now = db::now();

    let (episodes, retired) = {
        let conn = state.db.lock().await;
        (db::list_episodes(&conn, show.tvmaze_id)?, retired_releases(&conn))
    };

    let mut actionable: Vec<&EpisodeRow> = episodes
        .iter()
        .filter(|e| e.state == "wanted" && aired(e))
        .filter(|e| {
            if ignore_throttle {
                return true;
            }
            // fresh episodes retry every cycle; back catalog every 12h
            aired_within_days(e, 7.0) || now - e.last_searched_at > 12 * 3600
        })
        .collect();
    actionable.sort_by_key(|e| (e.season, e.number));

    if actionable.is_empty() {
        return Ok(PlanOutcome { plans: vec![], searched_ep_ids: vec![] });
    }

    let mut plans: Vec<PlannedGrab> = Vec::new();
    let mut searched_ep_ids: Vec<i64> = Vec::new();
    let mut by_season: std::collections::BTreeMap<i64, Vec<&EpisodeRow>> = Default::default();
    for e in &actionable {
        by_season.entry(e.season).or_default().push(e);
    }

    for (season, eps) in by_season {
        // Season pack first when it would cover several missing episodes.
        if profile.allow_season_packs && eps.len() >= 4 {
            let q = season_query(&show.name, season);
            let mut results = perform_search(state, &q, "tv", &[]).await?;
            results.releases.retain(|r| !retired.contains(r));
            if results.indexers.iter().any(|o| o.ok) {
                // the season query covered every wanted episode in this group
                searched_ep_ids.extend(eps.iter().map(|e| e.tvmaze_ep_id));
            }
            let season_size = episodes.iter().filter(|e| e.season == season).count() as i64;
            if let Some(pick) =
                pick_candidate(&results.releases, &profile, season, None, season_size.max(1))
            {
                plans.push(planned(show, season, &eps, pick, true));
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        for ep in eps {
            let q = episode_query(&show.name, season, ep.number);
            let mut results = perform_search(state, &q, "tv", &[]).await?;
            results.releases.retain(|r| !retired.contains(r));
            if results.indexers.iter().any(|o| o.ok) && !searched_ep_ids.contains(&ep.tvmaze_ep_id) {
                searched_ep_ids.push(ep.tvmaze_ep_id);
            }
            if let Some(pick) =
                pick_candidate(&results.releases, &profile, season, Some(ep.number), 1)
            {
                plans.push(planned(show, season, &[ep], pick, false));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    Ok(PlanOutcome { plans, searched_ep_ids })
}

fn planned(
    show: &ShowRow,
    season: i64,
    eps: &[&EpisodeRow],
    pick: &EnrichedRelease,
    is_pack: bool,
) -> PlannedGrab {
    PlannedGrab {
        show_id: show.tvmaze_id,
        show_name: show.name.clone(),
        season,
        episodes: eps.iter().map(|e| e.number).collect(),
        ep_ids: eps.iter().map(|e| e.tvmaze_ep_id).collect(),
        title: pick.release.title.clone(),
        indexer: pick.release.indexer.clone(),
        size: pick.release.size,
        seeders: pick.release.seeders,
        is_pack,
        magnet_url: pick.release.magnet_url.clone(),
        download_url: pick.release.download_url.clone(),
    }
}

async fn execute_plan(app: &tauri::AppHandle, state: &AppState, plan: &PlannedGrab) -> bool {
    let cfg = state.config.read().await.clone();
    let ck = crate::briefs::content_key(&plan.title);
    let (save_path, fresh_ep_ids) = {
        let conn = state.db.lock().await;
        let fresh: Vec<i64> = plan
            .ep_ids
            .iter()
            .filter(|id| {
                conn.query_row(
                    "SELECT COUNT(*) FROM episodes WHERE tvmaze_ep_id = ?1 AND state = 'wanted'",
                    [**id],
                    |r| r.get::<_, i64>(0),
                )
                .map(|n| n > 0)
                .unwrap_or(false)
            })
            .copied()
            .collect();
        if fresh.is_empty() {
            return false;
        }
        // The plan was built from a snapshot taken before slow searches - the
        // RSS sweep, the agent, a proposal, or the user may have satisfied it
        // meanwhile. The shared ledger is the source of truth; when it already
        // holds this content, link these episodes to that grab (a chat or
        // brief grab carries no episode ids) instead of leaving them wanted.
        if crate::db::ledger_satisfied(&conn, &ck) {
            if let Err(e) = crate::db::ledger_adopt_episodes(&conn, &ck, &fresh) {
                crate::applog::warn("scheduler", format!("could not link episodes to an existing grab: {e}"));
            }
            return false;
        }
        let sp = conn
            .query_row(
                "SELECT save_path_override FROM shows WHERE tvmaze_id = ?1",
                [plan.show_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        (sp, fresh)
    };
    let save_path = save_path.filter(|p| !p.is_empty()).or_else(|| {
        if cfg.save_path_tv.is_empty() { None } else { Some(cfg.save_path_tv.clone()) }
    });

    let outcome = crate::grab::dispatch(
        state,
        crate::grab::GrabOrder {
            title: plan.title.clone(),
            magnet_url: plan.magnet_url.clone(),
            download_url: plan.download_url.clone(),
            save_path,
            info_hash: magnet_hash(plan.magnet_url.as_deref()),
            size: plan.size,
        },
        None,
        fresh_ep_ids,
    )
    .await;

    match outcome {
        Ok(crate::grab::GrabOutcome::Grabbed { .. }) => {
            let what = if plan.is_pack {
                format!("{} S{:02} season pack", plan.show_name, plan.season)
            } else {
                format!("{} S{:02}E{:02}", plan.show_name, plan.season, plan.episodes[0])
            };
            let msg = format!(
                "Grabbed {what} · {:.1} GB{} · {}",
                plan.size as f64 / 1e9,
                plan.indexer.as_deref().map(|i| format!(" · {i}")).unwrap_or_default(),
                plan.title.chars().take(70).collect::<String>(),
            );
            let conn = state.db.lock().await;
            db::log_activity(&conn, "grab", Some(plan.show_id), &msg);
            drop(conn);
            crate::applog::info("scheduler", msg.clone());
            if cfg.notify_on_grab {
                use tauri_plugin_notification::NotificationExt;
                let _ = app.notification().builder().title("Trawler grabbed").body(&what).show();
            }
            crate::notify::dispatch(
                app,
                crate::notify::Kind::Grab,
                format!("Grabbed {what}"),
                format!(
                    "{:.1} GB{} · {}",
                    plan.size as f64 / 1e9,
                    plan.indexer.as_deref().map(|i| format!(" · {i}")).unwrap_or_default(),
                    plan.title.chars().take(90).collect::<String>()
                ),
            );
            true
        }
        // Another path grabbed (or is grabbing) this content — the ledger
        // covers the episodes, so this plan counts as handled.
        Ok(_) => false,
        Err(e) => {
            let msg = format!("Failed to grab {} S{:02}: {e}", plan.show_name, plan.season);
            let conn = state.db.lock().await;
            db::log_activity(&conn, "error", Some(plan.show_id), &msg);
            drop(conn);
            crate::applog::error("scheduler", msg.clone());
            false
        }
    }
}

/// Cross-cycle memory for "this ledger row's transfer is not in the backend
/// listing", shared by the qBittorrent and Bitport reapers so both apply
/// one rule. Absence becomes evidence only when it has been observed at
/// least twice AND for at least `WINDOW_SECS` of wall time: one odd listing
/// (a backend still loading its session after a restart, a pagination
/// surprise, an empty response) must not release every claim at once, and
/// two polls seconds apart (the tray's run-now plus the scheduled cycle)
/// are not two independent observations. A genuinely empty session — the
/// user deleted their only torrent — still confirms once the window passes.
#[derive(Default)]
struct AbsenceStrikes {
    /// row id → (first time seen missing, observations, any observation
    /// came from an empty listing)
    first_missing: std::collections::HashMap<i64, (i64, u32, bool)>,
}

impl AbsenceStrikes {
    const WINDOW_SECS: i64 = 10 * 60;
    /// A listing with nothing in it at all is weaker evidence: a backend
    /// mid-restart, an expired session, a partial page. It still counts —
    /// the user may really have removed their only transfer — but only
    /// after a longer stretch.
    const EMPTY_LISTING_WINDOW_SECS: i64 = 30 * 60;

    /// The row's transfer is in the listing (or the row has been settled):
    /// forget any strikes.
    fn present(&mut self, id: i64) {
        self.first_missing.remove(&id);
    }

    /// Record one absent observation; true when the row may be retired.
    /// The strike is kept until the caller settles the row with `present`,
    /// so a retire that fails (a busy database) is retried next cycle
    /// instead of restarting the window.
    fn missing(&mut self, id: i64, now: i64, listing_empty: bool) -> bool {
        let slot = self.first_missing.entry(id).or_insert((now, 0, false));
        slot.1 += 1;
        // a weak observation anywhere in the streak (not only the current
        // one) demands the longer window: an empty listing during a restart
        // followed by one normal miss is still one real observation
        slot.2 |= listing_empty;
        let window = if slot.2 { Self::EMPTY_LISTING_WINDOW_SECS } else { Self::WINDOW_SECS };
        slot.1 >= 2 && now - slot.0 >= window
    }
}

/// A grab whose swarm turned out to be dead — candidate for the medic.
pub struct DeadGrab {
    pub title: String,
    pub qbt_hash: String,
    pub size: i64,
    /// episodes the dead grab was for — the medic's replacement re-links them
    pub ep_ids: Vec<i64>,
}

/// Reopen ledger entries whose torrents are demonstrably dead — stuck fetching
/// metadata or stalled at 0 seeds for over an hour. Returns them so the medic
/// can hunt replacements.
///
/// "Over an hour" is measured from when Trawler FIRST saw the torrent
/// stalled with no seeds, not from when it was added: the old check reduced
/// to "no seeds at this instant" for anything older than an hour, so one
/// poll during a VPN drop paused every download and re-grabbed all of them.
/// Nothing is judged while qBittorrent itself is not connected.
fn reopen_dead_grabs(
    conn: &rusqlite::Connection,
    torrents: &[crate::qbit::QbitTorrent],
    qbt_connected: bool,
) -> Vec<DeadGrab> {
    // hash → first time this torrent was seen stalled with zero seeds; an
    // entry survives a disconnected cycle (the swarm didn't get healthier
    // because we lost our uplink) but is dropped as soon as the torrent moves
    static STALL_FIRST_SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, i64>>> =
        std::sync::OnceLock::new();
    let mut first_seen = STALL_FIRST_SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    reopen_dead_grabs_with(conn, torrents, qbt_connected, &mut first_seen, db::now())
}

fn reopen_dead_grabs_with(
    conn: &rusqlite::Connection,
    torrents: &[crate::qbit::QbitTorrent],
    qbt_connected: bool,
    first_seen: &mut std::collections::HashMap<String, i64>,
    now: i64,
) -> Vec<DeadGrab> {
    let mut dead = vec![];
    // an empty listing is a qBittorrent still loading its session, not a
    // world with no stalled torrents: judging on it would wipe every clock
    if !qbt_connected || torrents.is_empty() {
        return dead;
    }
    let stalled_now: std::collections::HashSet<&str> = torrents
        .iter()
        .filter(|t| t.state == "stalledDL" && t.num_seeds == 0 && t.dlspeed == 0)
        .map(|t| t.hash.as_str())
        .collect();
    first_seen.retain(|hash, _| stalled_now.contains(hash.as_str()));
    for t in torrents {
        // A live swarm serves metadata in seconds — 20 minutes of metaDL is a
        // ghost town. Stalled-with-data gets the longer benefit of the doubt.
        let dead_meta = t.state == "metaDL" && now - t.added_on > 20 * 60;
        let dead_stall = stalled_now.contains(t.hash.as_str()) && {
            let since = *first_seen.entry(t.hash.clone()).or_insert(now);
            now - since >= 3600
        };
        if !(dead_meta || dead_stall) {
            continue;
        }
        let norm = normalize(&t.name);
        // identity first: a magnet grab is renamed by qBittorrent once its
        // metadata lands, so the ledger title and the torrent name disagree
        // exactly when it matters; the title is the fallback for rows without
        // a comparable hash
        type OpenRow = (i64, String, i64, Option<String>, Option<String>);
        let hit: Option<(i64, String, i64, Option<String>)> = conn
            .prepare("SELECT id, title, size, ep_ids, info_hash FROM grab_ledger WHERE state = 'grabbed' AND backend = 'qbittorrent'")
            .ok()
            .and_then(|mut stmt| {
                stmt.query_map([], |r| {
                    Ok::<OpenRow, _>((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .ok()
                .and_then(|rows| {
                    // the title still counts when the hash disagrees: stale
                    // indexer definitions report wrong infohashes, and a row
                    // carrying one must not sit on a corpse forever
                    rows.flatten().find(|(_, title, _, _, info_hash)| {
                        let by_hash = info_hash
                            .as_deref()
                            .is_some_and(|h| qbt_comparable_hash(h) && h.eq_ignore_ascii_case(&t.hash));
                        by_hash || normalize(title) == norm
                    })
                })
                .map(|(id, title, size, ep_ids, _)| (id, title, size, ep_ids))
            });
        if let Some((id, title, size, ep_ids_raw)) = hit {
            let linked_ids = db::parse_ep_ids(ep_ids_raw.as_deref());
            let _ = conn.execute("UPDATE grab_ledger SET state = 'stalled' WHERE id = ?1", [id]);
            // hand the episode back to the scheduler: without this it sits in
            // 'grabbed' forever pointing at a dead release, and nothing —
            // medic off, agent unreachable, medic failure — ever frees it
            let _ = conn.execute(
                "UPDATE episodes SET state = 'wanted', grabbed_title = NULL, grabbed_at = NULL,
                        last_searched_at = 0
                 WHERE state = 'grabbed' AND grabbed_title = ?1",
                [&title],
            );
            db::set_episodes_state_by_ids(conn, &linked_ids, "wanted", None);
            // A stalled UPGRADE grab (same content key already completed once)
            // needs no medic — the user keeps the copy they already have, and
            // the agent would only refuse "already grabbed" replacements.
            let ck = crate::briefs::content_key(&title);
            let have_completed = conn
                .query_row(
                    "SELECT COUNT(*) FROM grab_ledger WHERE content_key = ?1 AND state = 'completed'",
                    [&ck],
                    |r| r.get::<_, i64>(0),
                )
                .map(|n| n > 0)
                .unwrap_or(false);
            if have_completed {
                db::log_activity(
                    conn,
                    "agent",
                    None,
                    &format!(
                        "Upgrade stalled: {} had no peers — keeping the copy you already have",
                        title.chars().take(70).collect::<String>()
                    ),
                );
                continue;
            }
            db::log_activity(
                conn,
                "error",
                None,
                &format!(
                    "Dead swarm: {} had no peers for over an hour — paused; the scheduler will look for another release",
                    title.chars().take(70).collect::<String>()
                ),
            );
            dead.push(DeadGrab {
                title,
                qbt_hash: t.hash.clone(),
                size: if size > 0 { size } else { t.size },
                ep_ids: linked_ids.clone(),
            });
        }
    }
    dead
}

/// The Stalled-Grab Medic: for each dead grab, pause the corpse (never delete)
/// and send the agent to find a live equivalent — same content, comparable
/// quality, healthiest swarm. Auto mode grabs; propose mode files a proposal.
async fn medic_pass(app: &tauri::AppHandle, state: &AppState, dead: Vec<DeadGrab>) {
    let cfg = state.config.read().await.clone();
    if cfg.medic_mode == "off" || !cfg.agent_enabled || dead.is_empty() {
        return;
    }
    let auto = cfg.medic_mode == "auto";

    // only the ones we actually handle this cycle; the rest stay detectable
    // (completion_pass already paused every corpse, whatever the medic mode)
    for item in dead.into_iter().take(2) {
        let parsed = crate::parse::parse(&item.title);
        // Equivalence guard, enforced in Rust: significant title tokens (and the
        // episode marker when present) must appear in any replacement.
        let mut include: Vec<String> = parsed
            .clean_title
            .split_whitespace()
            .filter(|w| w.len() > 2 || w.chars().all(|c| c.is_ascii_digit()))
            .take(4)
            .map(|w| w.to_lowercase())
            .collect();
        if let (Some(s), Some(e)) = (parsed.season, parsed.episode) {
            include.push(format!("s{s:02}e{e:02}"));
        }
        let plan = crate::briefs::HuntPlan {
            queries: vec![crate::follows::clean_show_name(&parsed.clean_title)],
            include,
            exclude: vec![],
            resolutions: parsed.resolution.clone().map(|r| vec![r]).unwrap_or_default(),
            max_size_gb: ((item.size as f64 / 1e9) * 2.0).max(3.0),
            min_seeders: 5,
            notes: String::new(),
        }
        .sanitize();

        let action_line = if auto {
            "If you find a live equivalent, grab it."
        } else {
            "If you find a live equivalent, propose it for the user's approval."
        };
        let system = format!(
            "{}\n\nYou are the stalled-grab medic. A grabbed release turned out to be a DEAD SWARM (no peers) and has been paused:\n  {}\nFind a LIVE release of the SAME content — same episode/event, comparable quality — with the healthiest swarm available (most seeders, minimum 5). Try one or two query variations. {}\nNever grab different content. If nothing equivalent is alive, conclude without action. Finish with one sentence.",
            crate::agent_run::SYSTEM_CORE,
            item.title.chars().take(120).collect::<String>(),
            action_line,
        );

        let mut ctx = crate::agent_tools::RunCtx::new(
            crate::agent_tools::RunOrigin::Medic,
            Some(plan),
            auto,
        );
        ctx.max_searches = 4;
        ctx.max_grabs = 1;
        ctx.medic_ep_ids = item.ep_ids.clone();
        ctx.max_gb = ((item.size as f64 / 1e9) * 2.0).max(3.0);

        let messages = vec![
            crate::llm::ChatMsg::system(system),
            crate::llm::ChatMsg::user("Find a replacement now."),
        ];
        let run_id = format!("medic-{}", db::now());
        let outcome = tokio::time::timeout(
            // beyond one full LLM call (llm.rs allows 300s per request) so a
            // slow-but-working model can complete a medic run
            std::time::Duration::from_secs(360),
            crate::agent_run::run(app, &mut ctx, messages, &run_id, false),
        )
        .await;

        let conn = state.db.lock().await;
        match outcome {
            Ok(Ok(o)) if o.grabs > 0 => {
                let msg = format!(
                    "Medic swapped a dead grab: {} → {}",
                    item.title.chars().take(50).collect::<String>(),
                    ctx.grabbed_titles.first().cloned().unwrap_or_default()
                );
                db::log_activity(&conn, "agent", None, &msg);
                crate::applog::info("scheduler", msg.clone());
                if cfg.notify_on_grab {
                    use tauri_plugin_notification::NotificationExt;
                    let _ = app
                        .notification()
                        .builder()
                        .title("Trawler medic")
                        .body("Swapped a dead grab for a live release")
                        .show();
                }
                crate::notify::dispatch(app, crate::notify::Kind::Grab, "Medic swapped a dead grab".into(), msg);
            }
            Ok(Ok(o)) if o.new_proposals > 0 => {
                db::log_activity(
                    &conn,
                    "agent",
                    None,
                    &format!(
                        "Medic found a live replacement for {} — waiting for your approval in the Agent view",
                        item.title.chars().take(50).collect::<String>()
                    ),
                );
                crate::notify::dispatch(
                    app,
                    crate::notify::Kind::Proposal,
                    "Medic found a replacement".into(),
                    format!(
                        "A live release replacing {} awaits your approval in the Agent view",
                        item.title.chars().take(70).collect::<String>()
                    ),
                );
            }
            Ok(Ok(_)) => {
                db::log_activity(
                    &conn,
                    "agent",
                    None,
                    &format!(
                        "Medic found no live equivalent of {} yet — will notice if one appears in a future search",
                        item.title.chars().take(50).collect::<String>()
                    ),
                );
            }
            Ok(Err(e)) => crate::applog::warn("scheduler",format!("medic run failed: {e}")),
            Err(_) => crate::applog::warn("scheduler", "medic run timed out"),
        }
    }
}

/// The btih out of a magnet link, lowercased — identity that survives
/// qBittorrent renaming the torrent once metadata arrives.
pub(crate) fn magnet_hash(magnet: Option<&str>) -> Option<String> {
    let url = url::Url::parse(magnet?).ok()?;
    let xt = url
        .query_pairs()
        .find(|(key, value)| key == "xt" && value.to_ascii_lowercase().starts_with("urn:btih:"))?
        .1;
    let raw = &xt[9..];
    if raw.len() == 40 && raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Some(raw.to_ascii_lowercase());
    }
    if raw.len() != 32 {
        return None;
    }
    // qBittorrent reports SHA-1 identities as 40 hex digits even when the
    // magnet supplied the equivalent 32-character base32 form.
    let mut decoded = Vec::with_capacity(20);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in raw.bytes() {
        let value = match byte.to_ascii_uppercase() {
            b'A'..=b'Z' => byte.to_ascii_uppercase() - b'A',
            b'2'..=b'7' => byte - b'2' + 26,
            _ => return None,
        } as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            decoded.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    if decoded.len() != 20 || bits != 0 {
        return None;
    }
    Some(decoded.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Cloud-side completion and reaping. A Bitport transfer reporting
/// "finished" flips its ledger row and linked episodes exactly like a
/// finished local torrent; a transfer that has VANISHED from the account
/// (deleted in Bitport's own UI, or an add that never stuck) releases its
/// row and episodes, mirroring the local orphan reaper. Rows are matched by
/// the transfer token stored at grab time; the magnet's btih and the title
/// cover rows without one.
async fn bitport_completion_pass(app: &tauri::AppHandle, state: &AppState) {
    let cfg = state.config.read().await.clone();
    if cfg.bitport_token.is_empty() {
        return;
    }
    let bp = crate::bitport::BitportClient { http: &state.http, token: cfg.bitport_token.clone() };
    let transfers = match bp.transfers().await {
        Ok(t) => t,
        Err(e) => {
            // no reaping on a failed poll — absence of evidence only counts
            // when the account actually answered
            crate::applog::warn("bitport", format!("transfer poll failed: {e}"));
            return;
        }
    };
    let done_tokens: std::collections::HashSet<&str> = transfers
        .iter()
        .filter(|t| t.status == "finished")
        .map(|t| t.token.as_str())
        .collect();
    let done_hashes: std::collections::HashSet<String> = transfers
        .iter()
        .filter(|t| t.status == "finished")
        .filter_map(crate::bitport::transfer_hash)
        .collect();
    let done_norms: std::collections::HashSet<String> = transfers
        .iter()
        .filter(|t| t.status == "finished")
        .map(|t| normalize(&t.name))
        .collect();
    let live_tokens: std::collections::HashSet<&str> =
        transfers.iter().map(|t| t.token.as_str()).collect();
    let live_hashes: std::collections::HashSet<String> =
        transfers.iter().filter_map(crate::bitport::transfer_hash).collect();
    let live_norms: std::collections::HashSet<String> =
        transfers.iter().map(|t| normalize(&t.name)).collect();
    // Absence is evidence only when it persists (see AbsenceStrikes): one
    // odd listing (pagination surprise, partial or empty response) must not
    // release every claim at once.
    static BP_STRIKES: std::sync::OnceLock<std::sync::Mutex<AbsenceStrikes>> =
        std::sync::OnceLock::new();
    let listing_empty = transfers.is_empty();
    let reap_cutoff = db::now() - 5 * 60; // covers the add-to-listing gap
    let recent_cutoff = db::now() - 3 * 86_400; // first cycle after an upgrade may flip a backlog
    let mut completed_display: Vec<String> = vec![];
    let conn = state.db.lock().await;
    // taken after the last .await: a std MutexGuard is !Send, and this
    // future runs under tauri::async_runtime::spawn
    let mut strikes = BP_STRIKES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    type BitportOpenRow =
        (i64, String, Option<String>, Option<String>, Option<String>, i64, String);
    let rows: Vec<BitportOpenRow> = conn
        .prepare("SELECT id, title, info_hash, ep_ids, bp_token, ts, state FROM grab_ledger WHERE state IN ('dispatching','grabbed') AND backend = 'bitport'")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })
            .map(|it| it.flatten().collect::<Vec<_>>())
            .unwrap_or_default()
        })
        .unwrap_or_default();
    if listing_empty && !rows.is_empty() {
        // an empty listing with open rows is suspicious, not evidence
        crate::applog::warn(
            "bitport",
            "transfer listing came back empty while cloud grabs are open — claims are released only if that persists for half an hour",
        );
    }
    for (id, title, info_hash, ep_ids_raw, bp_token, ts, ledger_state) in rows {
        let h = info_hash.map(|x| x.to_ascii_lowercase());
        let norm = normalize(&title);
        // a row WITH a token must not complete off an unrelated old transfer
        // that happens to share an infohash — token match is exact
        let is_done = match bp_token.as_deref() {
            Some(tok) => done_tokens.contains(tok),
            None => {
                h.as_ref().map(|x| done_hashes.contains(x)).unwrap_or(false)
                    || done_norms.contains(&norm)
            }
        };
        if is_done {
            strikes.present(id);
            let _ = conn.execute("UPDATE grab_ledger SET state = 'completed' WHERE id = ?1", [id]);
            db::set_episodes_state_by_ids(&conn, &db::parse_ep_ids(ep_ids_raw.as_deref()), "downloaded", None);
            db::log_activity(
                &conn,
                "complete",
                None,
                &format!("Finished in the cloud: {}", title.chars().take(60).collect::<String>()),
            );
            if ts >= recent_cutoff {
                completed_display.push(title.chars().take(60).collect());
            }
            continue;
        }
        let is_present = match bp_token.as_deref() {
            Some(tok) => live_tokens.contains(tok),
            None => {
                h.as_ref().map(|x| live_hashes.contains(x)).unwrap_or(false)
                    || live_norms.contains(&norm)
            }
        };
        if is_present {
            strikes.present(id);
            if ledger_state == "dispatching" {
                if let Err(error) = db::ledger_confirm_present(
                    &conn,
                    id,
                    &title,
                    &db::parse_ep_ids(ep_ids_raw.as_deref()),
                ) {
                    crate::applog::error(
                        "bitport",
                        format!("could not recover pending ledger row {id}: {error}"),
                    );
                }
            }
            continue;
        }
        // every absence counts, an empty listing included; the strike window
        // is what keeps a startup blip or a partial page from being evidence
        let confirmed = strikes.missing(id, db::now(), listing_empty);
        if ts < reap_cutoff && confirmed {
            if let Err(error) = db::ledger_confirm_missing(
                &conn,
                id,
                &db::parse_ep_ids(ep_ids_raw.as_deref()),
            ) {
                crate::applog::error(
                    "bitport",
                    format!("could not retire missing ledger row {id}: {error}"),
                );
                continue;
            }
            strikes.present(id);
            db::log_activity(
                &conn,
                "system",
                None,
                &format!(
                    "{} vanished from your Bitport cloud — its claim is released, Trawler can grab again",
                    title.chars().take(60).collect::<String>()
                ),
            );
        }
    }
    drop(strikes);
    drop(conn);
    if !completed_display.is_empty() {
        let (title, body) = if completed_display.len() == 1 {
            ("Finished in the cloud".to_string(), completed_display[0].clone())
        } else {
            (
                format!("{} cloud transfers finished", completed_display.len()),
                completed_display.iter().take(6).cloned().collect::<Vec<_>>().join("\n"),
            )
        };
        crate::notify::dispatch(app, crate::notify::Kind::Complete, title, body);
    }
}

/// Flip grabbed → downloaded by matching qBittorrent's finished torrents.
/// Returns any dead grabs detected, for the medic.
async fn completion_pass(app: &tauri::AppHandle, state: &AppState) -> Vec<DeadGrab> {
    let cfg = state.config.read().await.clone();
    let q = crate::qbit::QbitClient {
        http: &state.http,
        base: cfg.qbit_url.clone(),
        username: cfg.qbit_username.clone(),
        password: cfg.qbit_password.clone(),
    };
    // This pass matches OUR ledger rows against qBittorrent, so it lists the
    // FULL torrent set, not the category view: a torrent the user
    // recategorized must still count as present, or every open grab looks
    // "deleted" and Trawler re-grabs it in a loop.
    let torrents = match q.list(None).await {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    // Swarm doctor: only when the session is disconnected AND the current
    // listen port is provably inside a Windows-reserved range — that's a
    // diagnosis, not a guess. Rate-limited hard: a laptop that's merely
    // offline must never get its port churned or its activity feed spammed.
    let maindata = q.sync_maindata().await.ok();
    let connection_status = maindata
        .as_ref()
        .and_then(|md| md.pointer("/server_state/connection_status"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // the dead-swarm medic only judges torrents while qBittorrent has a
    // working uplink: "disconnected" means every torrent looks dead and none
    // of them is. No maindata at all (a reverse proxy that hides /sync) is
    // not evidence either way, so the medic keeps working on the list alone.
    let qbt_connected = connection_status != "disconnected";
    if maindata.is_none() {
        static SAID_IT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SAID_IT.swap(true, std::sync::atomic::Ordering::Relaxed) {
            crate::applog::warn(
                "qbit",
                "sync/maindata is unavailable — the swarm doctor is off and the medic cannot see the connection state",
            );
        }
    }
    if maindata.is_some() {
        let status = connection_status.as_str();
        if status == "disconnected" {
            let now = db::now();
            let (streak, last_move, attempts) = {
                let conn = state.db.lock().await;
                let streak: i64 = db::meta_get(&conn, "doctor_disc_streak")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
                    + 1;
                db::meta_set(&conn, "doctor_disc_streak", &streak.to_string());
                (
                    streak,
                    db::meta_get(&conn, "doctor_last_move").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0),
                    db::meta_get(&conn, "doctor_attempts").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0),
                )
            };
            let cur_port = q
                .preferences()
                .await
                .ok()
                .and_then(|p| p.get("listen_port").and_then(|v| v.as_u64()))
                .unwrap_or(0) as u16;
            let reserved = tokio::task::spawn_blocking(crate::setup::excluded_port_ranges)
                .await
                .unwrap_or_default();
            let port_is_the_problem =
                cur_port > 0 && reserved.iter().any(|(a, b)| cur_port >= *a && cur_port <= *b);
            if port_is_the_problem && streak >= 2 && now - last_move > 6 * 3600 && attempts < 3 {
                let new_port = tokio::task::spawn_blocking(crate::setup::pick_safe_listen_port)
                    .await
                    .unwrap_or(28645);
                match q.set_preferences(&serde_json::json!({ "listen_port": new_port })).await {
                    Ok(()) => {
                        crate::applog::warn(
                            "qbit",
                            format!("listen port {cur_port} sits in a Windows-reserved range — moved to {new_port}"),
                        );
                        let conn = state.db.lock().await;
                        db::meta_set(&conn, "doctor_last_move", &now.to_string());
                        db::meta_set(&conn, "doctor_attempts", &(attempts + 1).to_string());
                        db::log_activity(
                            &conn,
                            "system",
                            None,
                            &format!("qBittorrent's network port {cur_port} was blocked by Windows — Trawler moved it to {new_port}"),
                        );
                    }
                    Err(e) => crate::applog::error("qbit", format!("port move failed: {e}")),
                }
            } else if !port_is_the_problem && streak == 2 {
                // say it once per outage, not per cycle
                crate::applog::warn(
                    "qbit",
                    "session is disconnected but the listen port looks fine — likely offline/VPN; not touching anything",
                );
            }
        } else {
            let conn = state.db.lock().await;
            db::meta_set(&conn, "doctor_disc_streak", "0");
        }
    }
    let mut done: std::collections::HashSet<String> = torrents
        .iter()
        .filter(|t| is_complete_progress(t.progress))
        .map(|t| normalize(&t.name))
        .collect();
    let done_hashes: std::collections::HashSet<String> = torrents
        .iter()
        .filter(|t| is_complete_progress(t.progress))
        .map(|t| t.hash.to_ascii_lowercase())
        .collect();

    let mut completed_display: Vec<String> = vec![];
    let conn = state.db.lock().await;
    // NOTE: no early return on "nothing finished" — the resync and orphan
    // logic below must run even (especially) when everything is mid-download
    // magnet grabs: qBt renames the torrent once metadata lands, so recover
    // "this finished" via the ledger's infohash and fold it into the name set
    if !done_hashes.is_empty() {
        if let Ok(mut stmt) =
            conn.prepare("SELECT title, info_hash FROM grab_ledger WHERE info_hash IS NOT NULL AND backend = 'qbittorrent'")
        {
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map(|it| it.flatten().collect::<Vec<_>>())
                .unwrap_or_default();
            for (title, hash) in rows {
                if done_hashes.contains(&hash.to_ascii_lowercase()) {
                    done.insert(normalize(&title));
                }
            }
        }
    }
    // Orphan reaping: a grabbed ledger row whose torrent is nowhere in
    // qBittorrent means the user deleted it (in our Downloads view before
    // v0.3.7, or in qBittorrent itself). Holding the claim would block their
    // deliberate re-download forever — retire it and free the episodes. A
    // stale dispatching row is the crash-between-backend-and-finalize case;
    // the same authoritative listing either recovers or retires it.
    {
        let all_norms: std::collections::HashSet<String> =
            torrents.iter().map(|t| normalize(&t.name)).collect();
        let all_hashes: std::collections::HashSet<String> =
            torrents.iter().map(|t| t.hash.to_ascii_lowercase()).collect();
        // covers the add-to-listing gap with room for a slow dispatch: the
        // .torrent fetch through Prowlarr can follow five 60 s hops before the
        // add even starts, and a claim reaped mid-flight would let the add
        // land with no ledger row to finish (a duplicate grab next cycle)
        let cutoff = db::now() - 15 * 60;
        // a stalled row is retired too once its torrent is gone: the user
        // deleted the corpse, so the release may be picked again
        type ReapRow = (i64, String, Option<String>, Option<String>, String);
        let rows: Vec<ReapRow> = conn
            .prepare("SELECT id, title, info_hash, ep_ids, state FROM grab_ledger WHERE state IN ('dispatching','grabbed','stalled') AND ts < ?1 AND backend = 'qbittorrent'")
            .ok()
            .map(|mut stmt| {
                stmt.query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                    .map(|it| it.flatten().collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        // Absence is evidence only when it persists (see AbsenceStrikes).
        // qBittorrent answers the WebUI before it has finished loading resume
        // data after a restart, so one listing can be missing every torrent;
        // reaping on it released every claim at once and the next cycle
        // re-grabbed all of them.
        static QBT_STRIKES: std::sync::OnceLock<std::sync::Mutex<AbsenceStrikes>> =
            std::sync::OnceLock::new();
        let mut strikes = QBT_STRIKES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if torrents.is_empty() && !rows.is_empty() {
            crate::applog::warn(
                "scheduler",
                "qBittorrent listed no torrents while grabs are open — claims are released only if that persists",
            );
        }
        let now = db::now();
        let listing_empty = torrents.is_empty();
        for (id, title, info_hash, ep_ids_raw, ledger_state) in rows {
            let hash_present = info_hash
                .map(|h| all_hashes.contains(&h.to_ascii_lowercase()))
                .unwrap_or(false);
            if hash_present || all_norms.contains(&normalize(&title)) {
                strikes.present(id);
                continue;
            }
            if !strikes.missing(id, now, listing_empty) {
                continue;
            }
            // a stalled row's episodes were handed back when it stalled and
            // may belong to a replacement grab by now - never touch them here
            let was_stalled = ledger_state == "stalled";
            let linked = if was_stalled { vec![] } else { db::parse_ep_ids(ep_ids_raw.as_deref()) };
            if let Err(error) = db::ledger_confirm_missing(&conn, id, &linked) {
                crate::applog::error(
                    "scheduler",
                    format!("could not retire missing ledger row {id}: {error}"),
                );
                continue;
            }
            strikes.present(id);
            if was_stalled {
                continue;
            }
            let _ = conn.execute(
                "UPDATE episodes SET state = 'wanted', grabbed_title = NULL, grabbed_at = NULL,
                        last_searched_at = 0
                 WHERE state = 'grabbed' AND grabbed_title = ?1",
                [&title],
            );
            db::log_activity(
                &conn,
                "system",
                None,
                &format!(
                    "{} vanished from qBittorrent — its claim is released, Trawler can grab again",
                    title.chars().take(60).collect::<String>()
                ),
            );
        }
    }
    let dead = reopen_dead_grabs(&conn, &torrents, qbt_connected);
    // display names dedupe on the normalized torrent title, since an episode
    // grab appears both in episodes and in the shared ledger
    let mut seen_norms: std::collections::HashSet<String> = Default::default();

    // Only grabs from the last few days are worth announcing — the first
    // cycle after an upgrade may flip a backlog of old ledger rows silently.
    let recent_cutoff = db::now() - 3 * 86_400;

    let grabbed: Vec<(i64, String, i64)> = {
        let mut stmt = match conn.prepare(
            "SELECT tvmaze_ep_id, grabbed_title, COALESCE(grabbed_at, 0) FROM episodes
             WHERE state = 'grabbed' AND grabbed_title IS NOT NULL",
        ) {
            Ok(s) => s,
            Err(_) => return dead,
        };
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))
            .map(|it| it.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        rows
    };
    // group by torrent so a finished season pack reads as one item, not E01
    let mut ep_groups: std::collections::BTreeMap<String, (String, i64, Vec<i64>)> = Default::default();
    for (ep_id, title, grabbed_at) in grabbed {
        let norm = normalize(&title);
        if done.contains(&norm) {
            let _ = conn.execute(
                "UPDATE episodes SET state = 'downloaded' WHERE tvmaze_ep_id = ?1",
                [ep_id],
            );
            seen_norms.insert(norm.clone());
            if grabbed_at >= recent_cutoff {
                if let Ok((name, season, number)) = conn.query_row(
                    "SELECT s.name, e.season, e.number FROM episodes e
                     JOIN shows s ON s.tvmaze_id = e.show_id WHERE e.tvmaze_ep_id = ?1",
                    [ep_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
                ) {
                    let entry = ep_groups.entry(norm).or_insert((name, season, vec![]));
                    entry.2.push(number);
                }
            }
        }
    }
    for (_, (name, season, eps)) in ep_groups {
        match eps.as_slice() {
            [only] => completed_display.push(format!("{name} S{season:02}E{only:02}")),
            many => completed_display.push(format!("{name} S{season:02} ({} episodes)", many.len())),
        }
    }

    // the shared ledger covers brief/agent grabs that have no episode row —
    // and rows carrying ep_ids are the durable episode linkage that survives
    // an unfollow/refollow wiping grabbed_title
    let running_norms: std::collections::HashSet<String> =
        torrents.iter().filter(|t| !is_complete_progress(t.progress)).map(|t| normalize(&t.name)).collect();
    let running_hashes: std::collections::HashSet<String> =
        torrents.iter().filter(|t| !is_complete_progress(t.progress)).map(|t| t.hash.to_ascii_lowercase()).collect();
    // a stalled row is included so a dead swarm that came back to life and
    // finished still flips to completed (and its episodes to downloaded)
    type QbitOpenRow = (i64, String, i64, Option<String>, Option<String>, String);
    let open_ledger: Vec<QbitOpenRow> = conn
        .prepare("SELECT id, title, ts, info_hash, ep_ids, state FROM grab_ledger WHERE state IN ('dispatching','grabbed','stalled') AND backend = 'qbittorrent'")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .map(|it| it.flatten().collect::<Vec<_>>())
            .unwrap_or_default()
        })
        .unwrap_or_default();
    for (id, title, ts, info_hash, ep_ids_raw, ledger_state) in open_ledger {
        let norm = normalize(&title);
        let linked = db::parse_ep_ids(ep_ids_raw.as_deref());
        // hash first — magnet grabs get renamed by qBt once metadata lands,
        // so the listing title and the torrent name often disagree
        let h = info_hash.map(|x| x.to_ascii_lowercase());
        let hash_done = h.as_ref().map(|x| done_hashes.contains(x)).unwrap_or(false);
        if hash_done || done.contains(&norm) {
            let _ = conn.execute("UPDATE grab_ledger SET state = 'completed' WHERE id = ?1", [id]);
            // by-id flip catches episodes whose grabbed_title linkage was lost
            db::set_episodes_state_by_ids(&conn, &linked, "downloaded", None);
            if seen_norms.insert(norm) && ts >= recent_cutoff {
                completed_display.push(title.chars().take(60).collect());
            }
            continue;
        }
        // a paused corpse is neither running nor to be re-linked: its
        // episodes were deliberately handed back to the scheduler
        if ledger_state == "stalled" {
            continue;
        }
        // resync: the torrent is still running but a refollow reset its
        // episodes to 'wanted' — put the truth back on the screen (and keep
        // the scheduler from grabbing a duplicate)
        let hash_running = h.as_ref().map(|x| running_hashes.contains(x)).unwrap_or(false);
        if hash_running || running_norms.contains(&norm) {
            if let Err(error) = db::ledger_confirm_present(&conn, id, &title, &linked) {
                crate::applog::error(
                    "scheduler",
                    format!("could not recover pending ledger row {id}: {error}"),
                );
            } else if !linked.is_empty() {
                // Also repairs linked episodes after an unfollow/refollow when
                // the ledger was already finalized before this pass.
                db::set_episodes_state_by_ids(&conn, &linked, "grabbed", Some(&title));
            }
        }
    }

    for item in &completed_display {
        db::log_activity(&conn, "complete", None, &format!("Finished downloading {item}"));
    }
    drop(conn);

    // a dead swarm is paused whatever the medic mode - Settings promises
    // "Off: just pause and tell me", and a corpse left running keeps its
    // half-downloaded files busy while the scheduler fetches a replacement
    for d in &dead {
        if let Err(e) = q.torrent_action("stop", &d.qbt_hash).await {
            crate::applog::warn("scheduler", format!("could not pause dead torrent {}: {e}", d.qbt_hash));
        }
    }

    if !completed_display.is_empty() {
        let (title, body) = if completed_display.len() == 1 {
            ("Download complete".to_string(), completed_display[0].clone())
        } else {
            (
                format!("{} downloads complete", completed_display.len()),
                completed_display.iter().take(6).cloned().collect::<Vec<_>>().join("\n"),
            )
        };
        crate::notify::dispatch(app, crate::notify::Kind::Complete, title, body);
    }
    notify_dead(app, &dead);
    dead
}

/// Dead swarms are worth a push even when the medic is off — batched into
/// one message so a qBittorrent hiccup can't burn the hourly rate cap.
fn notify_dead(app: &tauri::AppHandle, dead: &[DeadGrab]) {
    if dead.is_empty() {
        return;
    }
    let title = if dead.len() == 1 {
        "Dead swarm".to_string()
    } else {
        format!("{} dead swarms", dead.len())
    };
    let body = dead
        .iter()
        .take(6)
        .map(|d| d.title.chars().take(80).collect::<String>())
        .collect::<Vec<_>>()
        .join("
");
    crate::notify::dispatch(
        app,
        crate::notify::Kind::Error,
        title,
        format!("No peers for over an hour — paused:
{body}"),
    );
}

/// One full scheduler cycle over every followed show. Returns grab count.
pub async fn run_cycle(app: &tauri::AppHandle) -> Result<usize> {
    let state_guard = app.state::<AppState>();
    let state: &AppState = state_guard.inner();

    if state.scheduler_busy.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Ok(0); // a cycle is already running
    }
    // drop guard: a panic inside the cycle must not leave the flag stuck,
    // which would silently disable the scheduler for the whole session
    struct Busy<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for Busy<'_> {
        fn drop(&mut self) {
            self.0.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _busy = Busy(&state.scheduler_busy);
    run_cycle_inner(app, state).await
}

async fn run_cycle_inner(app: &tauri::AppHandle, state: &AppState) -> Result<usize> {
    let shows = {
        let conn = state.db.lock().await;
        // newly aired episodes become wanted immediately, not on the 20h refresh
        crate::rss::promote_aired(&conn);
        db::list_shows(&conn)?
    };
    // completion + resync FIRST: the planner must see post-resync episode
    // states, or the first cycle after a refollow can double-grab a season
    // pack whose episodes still read as wanted
    let dead = completion_pass(app, state).await;
    medic_pass(app, state, dead).await;
    bitport_completion_pass(app, state).await;

    let now = db::now();
    let mut grabs = 0usize;
    // the follow path gets the same rails briefs have: a per-cycle grab cap
    // and a free-disk floor, so a catalog import can't flood the disk
    const MAX_GRABS_PER_CYCLE: usize = 6;
    let disk_ok = {
        let cfg = state.config.read().await.clone();
        match crate::grab::selected_backend_free_bytes(&state.http, &cfg).await {
            Ok(free) => (free as f64) >= crate::grab::min_free_bytes(&cfg),
            Err(error) => {
                crate::applog::warn(
                    "scheduler",
                    format!("could not verify selected-backend capacity — no automatic grabs this cycle ({error})"),
                );
                false
            }
        }
    };
    if !disk_ok {
        crate::applog::warn(
            "scheduler",
            "selected-backend capacity is below the floor or could not be verified — no grabs this cycle",
        );
    }
    crate::applog::info(
        "scheduler",
        format!(
            "cycle: {} show(s), {} wanted episode(s)",
            shows.len(),
            shows.iter().map(|s| s.wanted).sum::<i64>()
        ),
    );

    for show in &shows {
        if now - show.refreshed_at > 20 * 3600 && (show.status != "Ended" || show.wanted > 0) {
            if let Err(e) = refresh_show(state, show.tvmaze_id).await {
                crate::applog::warn("scheduler",format!("refresh {} failed: {e}", show.name));
            }
        }
        if show.status == "Ended" && show.wanted == 0 {
            continue; // dormant
        }
        // nothing can be grabbed this cycle: searching anyway only loads the
        // indexers and stamps the back catalog into its 12h throttle
        if !disk_ok {
            continue;
        }

        match plan_for_show(state, show, false).await {
            Ok(outcome) => {
                if !outcome.searched_ep_ids.is_empty() {
                    // stamp ONLY what was actually searched with indexers
                    // answering — stamping unsearched episodes starves them
                    let conn = state.db.lock().await;
                    db::stamp_searched(&conn, &outcome.searched_ep_ids);
                }
                for plan in &outcome.plans {
                    if grabs >= MAX_GRABS_PER_CYCLE {
                        break;
                    }
                    if execute_plan(app, state, plan).await {
                        grabs += 1;
                    }
                }
                if grabs >= MAX_GRABS_PER_CYCLE {
                    break;
                }
            }
            Err(e) => crate::applog::warn("scheduler",format!("planning {} failed: {e}", show.name)),
        }
    }

    Ok(grabs)
}

pub async fn scheduler_loop(app: tauri::AppHandle) {
    // let the app settle before the first cycle
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    loop {
        match run_cycle(&app).await {
            Ok(n) if n > 0 => crate::applog::info("scheduler",format!("scheduler cycle done: {n} grabs")),
            Ok(_) => {}
            Err(e) => crate::applog::warn("scheduler",format!("scheduler cycle error: {e}")),
        }
        let minutes = {
            let state = app.state::<AppState>();
            let m = state.config.read().await.scheduler_minutes;
            m.clamp(5, 24 * 60) as u64
        };
        tokio::time::sleep(std::time::Duration::from_secs(minutes * 60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use crate::prowlarr::ProwlarrRelease;

    fn release(title: &str, size: i64, seeders: i32) -> EnrichedRelease {
        let parsed = parse::parse(title);
        EnrichedRelease {
            release: ProwlarrRelease {
                guid: None,
                title: title.into(),
                size,
                indexer: Some("Test".into()),
                indexer_id: 1,
                info_url: None,
                download_url: None,
                magnet_url: Some("magnet:?xt=urn:btih:0".into()),
                info_hash: None,
                seeders: Some(seeders),
                leechers: Some(1),
                protocol: Some("torrent".into()),
                publish_date: None,
                age: 1,
                grabs: None,
                categories: vec![],
                imdb_id: None,
                tmdb_id: None,
                tvdb_id: None,
            },
            score: seeders as f64 * 0.3,
            dupe_count: 1,
            also_on: vec![],
            kind: "tv".into(),
            relevant: true,
            parsed,
        }
    }

    #[test]
    fn magnet_identity_is_persisted_in_qbittorrents_hex_form() {
        assert_eq!(
            magnet_hash(Some(
                "magnet:?dn=renamed&xt=urn%3Abtih%3A0123456789ABCDEF0123456789ABCDEF01234567"
            )),
            Some("0123456789abcdef0123456789abcdef01234567".into())
        );
        assert_eq!(
            magnet_hash(Some("magnet:?xt=urn:btih:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")),
            Some("0000000000000000000000000000000000000000".into())
        );
        assert_eq!(magnet_hash(Some("magnet:?xt=urn:btih:not-a-hash")), None);
    }

    #[test]
    fn completion_requires_all_bytes() {
        assert!(!is_complete_progress(0.999));
        assert!(!is_complete_progress(0.999_999));
        assert!(is_complete_progress(1.0));
    }

    #[test]
    fn only_qbittorrent_hash_forms_override_title_matching() {
        assert!(qbt_comparable_hash("0123456789abcdef0123456789abcdef01234567"));
        assert!(qbt_comparable_hash(&"a".repeat(64)));
        assert!(!qbt_comparable_hash("ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"));
        assert!(!qbt_comparable_hash(""));
    }

    fn ledger_mem() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE grab_ledger (
               id INTEGER PRIMARY KEY AUTOINCREMENT, content_key TEXT NOT NULL,
               brief_id INTEGER, title TEXT NOT NULL, info_hash TEXT, size INTEGER NOT NULL DEFAULT 0,
               state TEXT NOT NULL, ts INTEGER NOT NULL, ep_ids TEXT,
               backend TEXT NOT NULL DEFAULT 'qbittorrent', bp_token TEXT
             );
             CREATE TABLE episodes (
               tvmaze_ep_id INTEGER PRIMARY KEY, state TEXT NOT NULL,
               grabbed_title TEXT, grabbed_at INTEGER, last_searched_at INTEGER DEFAULT 0
             );
             CREATE TABLE activity (
               id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
               kind TEXT NOT NULL, show_id INTEGER, message TEXT NOT NULL
             );",
        )
        .unwrap();
        conn
    }

    fn qbt(name: &str, hash: &str, state: &str, seeds: i64, age_secs: i64) -> crate::qbit::QbitTorrent {
        crate::qbit::QbitTorrent {
            hash: hash.into(),
            name: name.into(),
            size: 1_000,
            progress: 0.0,
            dlspeed: 0,
            upspeed: 0,
            eta: 0,
            state: state.into(),
            category: String::new(),
            save_path: String::new(),
            added_on: db::now() - age_secs,
            num_seeds: seeds,
            num_leechs: 0,
            ratio: 0.0,
            content_path: String::new(),
            auto_tmm: false,
        }
    }

    #[test]
    fn dead_magnet_grab_is_found_by_hash_after_qbittorrent_renamed_it() {
        let conn = ledger_mem();
        let hash = "0123456789abcdef0123456789abcdef01234567";
        conn.execute(
            "INSERT INTO grab_ledger (content_key, title, info_hash, size, state, ts, ep_ids)
             VALUES ('tv:show:s01e01', 'Show.S01E01.1080p.WEB-GRP', ?1, 0, 'grabbed', ?2, '[7]')",
            rusqlite::params![hash, db::now()],
        )
        .unwrap();
        conn.execute("INSERT INTO episodes (tvmaze_ep_id, state, grabbed_title) VALUES (7, 'grabbed', 'Show.S01E01.1080p.WEB-GRP')", []).unwrap();
        // metadata landed: qBittorrent now shows the torrent's real name
        let torrents = vec![qbt("Show S01E01 (real folder name)", hash, "stalledDL", 0, 2 * 3600)];
        let now = db::now();
        // stalled with no seeds for an hour of observed wall time
        let mut seen = std::collections::HashMap::from([(hash.to_string(), now - 3600)]);
        let dead = reopen_dead_grabs_with(&conn, &torrents, true, &mut seen, now);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].qbt_hash, hash);
        assert_eq!(dead[0].ep_ids, vec![7]);
        let state: String = conn
            .query_row("SELECT state FROM grab_ledger WHERE info_hash = ?1", [hash], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "stalled");
        let ep: String = conn
            .query_row("SELECT state FROM episodes WHERE tvmaze_ep_id = 7", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ep, "wanted");
    }

    #[test]
    fn dead_grab_with_a_wrong_indexer_hash_is_still_found_by_title() {
        // stale indexer definitions report a wrong infoHash; the ledger then
        // carries a hash qBittorrent never shows, and the title must still work
        let conn = ledger_mem();
        conn.execute(
            "INSERT INTO grab_ledger (content_key, title, info_hash, size, state, ts, ep_ids)
             VALUES ('tv:show:s01e02', 'Show.S01E02.1080p.WEB-GRP', ?1, 0, 'grabbed', ?2, '[8]')",
            rusqlite::params!["f".repeat(40), db::now()],
        )
        .unwrap();
        conn.execute("INSERT INTO episodes (tvmaze_ep_id, state, grabbed_title) VALUES (8, 'grabbed', 'Show.S01E02.1080p.WEB-GRP')", []).unwrap();
        let torrents = vec![qbt("Show.S01E02.1080p.WEB-GRP", &"a".repeat(40), "metaDL", 0, 30 * 60)];
        let mut seen = std::collections::HashMap::new();
        let dead = reopen_dead_grabs_with(&conn, &torrents, true, &mut seen, db::now());
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].ep_ids, vec![8]);
    }

    #[test]
    fn absence_needs_two_observations_and_ten_minutes() {
        let t0 = 1_000_000;
        let w = AbsenceStrikes::WINDOW_SECS;
        // first sighting never confirms
        let mut s = AbsenceStrikes::default();
        assert!(!s.missing(7, t0, false));
        // a second poll seconds later (run-now + scheduled cycle) is not
        // an independent observation
        assert!(!s.missing(7, t0 + 5, false));
        // exactly two observations spanning the window confirm
        let mut two = AbsenceStrikes::default();
        assert!(!two.missing(8, t0, false));
        assert!(two.missing(8, t0 + w, false));
        // two observations one second short of the window do not
        let mut short = AbsenceStrikes::default();
        assert!(!short.missing(8, t0, false));
        assert!(!short.missing(8, t0 + w - 1, false));
        // the strike survives a confirm until the caller settles the row,
        // so a failed retire is retried next cycle rather than re-windowed
        assert!(two.missing(8, t0 + w + 1, false));
        two.present(8);
        assert!(!two.missing(8, t0 + 3 * w, false));
        // seen again in between: strikes are forgotten
        let mut back = AbsenceStrikes::default();
        assert!(!back.missing(9, t0, false));
        back.present(9);
        assert!(!back.missing(9, t0 + 2 * w, false));
        // an empty listing needs the longer window
        let mut empty = AbsenceStrikes::default();
        assert!(!empty.missing(10, t0, true));
        assert!(!empty.missing(10, t0 + w, true));
        assert!(empty.missing(10, t0 + AbsenceStrikes::EMPTY_LISTING_WINDOW_SECS, true));
        // a weak first observation followed by one normal miss is still one
        // real observation: the longer window applies to the whole streak
        let mut weak_first = AbsenceStrikes::default();
        assert!(!weak_first.missing(11, t0, true));
        assert!(!weak_first.missing(11, t0 + w, false));
        assert!(weak_first.missing(11, t0 + AbsenceStrikes::EMPTY_LISTING_WINDOW_SECS, false));
    }

    #[test]
    fn empty_listing_does_not_touch_stall_clocks() {
        let conn = ledger_mem();
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let now = db::now();
        let mut seen = std::collections::HashMap::from([(hash.to_string(), now - 3000)]);
        // qBittorrent answering before its session has loaded
        assert!(reopen_dead_grabs_with(&conn, &[], true, &mut seen, now).is_empty());
        assert_eq!(seen.get(hash), Some(&(now - 3000)));
    }

    #[test]
    fn a_single_stalled_sample_does_not_kill_a_grab() {
        // regression: "stalled, 0 seeds, added over an hour ago" used to be
        // enough on its own — one offline poll paused every download
        let conn = ledger_mem();
        let hash = "0123456789abcdef0123456789abcdef01234567";
        conn.execute(
            "INSERT INTO grab_ledger (content_key, title, info_hash, size, state, ts, ep_ids)
             VALUES ('tv:show:s01e01', 'Show.S01E01.1080p.WEB-GRP', ?1, 0, 'grabbed', ?2, '[7]')",
            rusqlite::params![hash, db::now()],
        )
        .unwrap();
        let torrents = vec![qbt("Show.S01E01.1080p.WEB-GRP", hash, "stalledDL", 0, 5 * 3600)];
        let now = db::now();
        let mut seen = std::collections::HashMap::new();
        // first sighting only starts the clock
        assert!(reopen_dead_grabs_with(&conn, &torrents, true, &mut seen, now).is_empty());
        assert_eq!(seen.get(hash), Some(&now));
        // 30 minutes later, still not enough
        assert!(reopen_dead_grabs_with(&conn, &torrents, true, &mut seen, now + 1800).is_empty());
        // an hour after first sighting: dead
        assert_eq!(reopen_dead_grabs_with(&conn, &torrents, true, &mut seen, now + 3600).len(), 1);
    }

    #[test]
    fn nothing_is_judged_while_qbittorrent_is_disconnected() {
        let conn = ledger_mem();
        let hash = "0123456789abcdef0123456789abcdef01234567";
        conn.execute(
            "INSERT INTO grab_ledger (content_key, title, info_hash, size, state, ts, ep_ids)
             VALUES ('tv:show:s01e01', 'Show.S01E01.1080p.WEB-GRP', ?1, 0, 'grabbed', ?2, '[7]')",
            rusqlite::params![hash, db::now()],
        )
        .unwrap();
        let torrents = vec![qbt("Show.S01E01.1080p.WEB-GRP", hash, "stalledDL", 0, 5 * 3600)];
        let now = db::now();
        let mut seen = std::collections::HashMap::from([(hash.to_string(), now - 7200)]);
        assert!(reopen_dead_grabs_with(&conn, &torrents, false, &mut seen, now).is_empty());
        // the clock is kept, not reset, across the outage
        assert_eq!(seen.get(hash), Some(&(now - 7200)));
        let state: String = conn
            .query_row("SELECT state FROM grab_ledger WHERE info_hash = ?1", [hash], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "grabbed");
    }

    #[test]
    fn a_torrent_that_moves_again_resets_its_stall_clock() {
        let conn = ledger_mem();
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let now = db::now();
        let mut seen = std::collections::HashMap::from([(hash.to_string(), now - 3000)]);
        let mut moving = qbt("Show.S01E01.1080p.WEB-GRP", hash, "downloading", 3, 5 * 3600);
        moving.dlspeed = 50_000;
        assert!(reopen_dead_grabs_with(&conn, &[moving], true, &mut seen, now).is_empty());
        assert!(!seen.contains_key(hash));
    }

    #[test]
    fn stalled_releases_are_excluded_from_planning() {
        let conn = ledger_mem();
        let hash = "0123456789abcdef0123456789abcdef01234567";
        conn.execute(
            "INSERT INTO grab_ledger (content_key, title, info_hash, size, state, ts)
             VALUES ('tv:show:s01e01', 'Show.S01E01.1080p.WEB-DEAD', ?1, 0, 'stalled', ?2)",
            rusqlite::params![hash, db::now()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO grab_ledger (content_key, title, info_hash, size, state, ts)
             VALUES ('tv:show:s01e02', 'Show.S01E02.1080p.WEB-FINE', NULL, 0, 'grabbed', ?1)",
            [db::now()],
        )
        .unwrap();
        let retired = retired_releases(&conn);
        // the dead release is skipped by title...
        let mut r = release("Show.S01E01.1080p.WEB-DEAD", 1_000, 50);
        r.release.magnet_url = None;
        assert!(retired.contains(&r));
        // ...and by identity when the indexer lists it under another name
        let mut r2 = release("Show.S01E01.1080p.WEB-DEAD.REPOST", 1_000, 50);
        r2.release.info_hash = Some(hash.to_uppercase());
        assert!(retired.contains(&r2));
        // a live grab and unrelated releases are untouched
        assert!(!retired.contains(&release("Show.S01E02.1080p.WEB-FINE", 1_000, 50)));
        assert!(!retired.contains(&release("Show.S01E01.1080p.WEB-OTHER", 1_000, 50)));
    }

    #[test]
    fn picks_matching_episode_not_pack() {
        let profile = QualityProfile::default();
        let rs = vec![
            release("Show.S02.COMPLETE.1080p.WEB-DL.x265-A", 20_000_000_000, 50),
            release("Show.S02E05.1080p.WEB-DL.x265-B", 2_000_000_000, 30),
            release("Show.S02E06.1080p.WEB-DL.x265-C", 2_000_000_000, 90),
            release("Show.S03E05.1080p.WEB-DL.x265-D", 2_000_000_000, 500),
        ];
        let pick = pick_candidate(&rs, &profile, 2, Some(5), 1).unwrap();
        assert!(pick.release.title.contains("S02E05"));
    }

    #[test]
    fn picks_pack_when_asked() {
        let profile = QualityProfile::default();
        let rs = vec![
            release("Show.S02E05.1080p.WEB-DL.x265-B", 2_000_000_000, 300),
            release("Show S02 Complete 1080p WEB-DL x265-A", 18_000_000_000, 40),
        ];
        let pick = pick_candidate(&rs, &profile, 2, None, 9).unwrap();
        assert!(pick.parsed.season_pack);
    }

    #[test]
    fn respects_resolution_filter() {
        let profile = QualityProfile {
            resolutions: vec!["1080p".into()],
            ..Default::default()
        };
        let rs = vec![
            release("Show.S01E01.2160p.WEB-DL.x265-A", 6_000_000_000, 400),
            release("Show.S01E01.720p.WEB-DL.x264-B", 900_000_000, 200),
            release("Show.S01E01.1080p.WEB-DL.x264-C", 2_000_000_000, 100),
        ];
        let pick = pick_candidate(&rs, &profile, 1, Some(1), 1).unwrap();
        assert_eq!(pick.parsed.resolution.as_deref(), Some("1080p"));
    }

    #[test]
    fn prefers_x265_via_boost() {
        let profile = QualityProfile::default(); // prefer-x265
        let rs = vec![
            release("Show.S01E01.1080p.WEB-DL.x264-A", 2_000_000_000, 110),
            release("Show.S01E01.1080p.WEB-DL.x265-B", 2_000_000_000, 100),
        ];
        // near-equal seeders: the x265 boost should win
        let pick = pick_candidate(&rs, &profile, 1, Some(1), 1).unwrap();
        assert_eq!(pick.parsed.codec.as_deref(), Some("x265"));
    }

    #[test]
    fn skips_dead_torrents() {
        let profile = QualityProfile::default();
        let rs = vec![release("Show.S01E01.1080p.WEB-DL.x265-A", 2_000_000_000, 0)];
        assert!(pick_candidate(&rs, &profile, 1, Some(1), 1).is_none());
    }
}
