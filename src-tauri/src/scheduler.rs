//! The follow scheduler: plans and executes grabs for followed shows.

use chrono::Utc;
use serde::Serialize;
use tauri::Manager;

use crate::commands::{normalize, perform_grab, perform_search, EnrichedRelease};
use crate::config::QualityProfile;
use crate::db::{self, EpisodeRow, ShowRow};
use crate::error::Result;
use crate::follows::{codec_boost, episode_query, profile_allows, refresh_show, season_query};
use crate::AppState;

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

    let episodes = {
        let conn = state.db.lock().await;
        db::list_episodes(&conn, show.tvmaze_id)?
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
            let results = perform_search(state, &q, "tv", &[]).await?;
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
            let results = perform_search(state, &q, "tv", &[]).await?;
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
        // The plan was built from a snapshot taken before slow searches — the
        // RSS sweep (or the user) may have satisfied it meanwhile. The shared
        // ledger is the source of truth for both paths.
        if crate::db::ledger_satisfied(&conn, &ck) {
            return false;
        }
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

    let result = perform_grab(
        state,
        &plan.title,
        plan.magnet_url.clone(),
        plan.download_url.clone(),
        save_path,
    )
    .await;

    let conn = state.db.lock().await;
    match result {
        Ok(_) => {
            db::mark_grabbed(&conn, &fresh_ep_ids, &plan.title);
            db::ledger_insert(&conn, &ck, None, &plan.title, magnet_hash(plan.magnet_url.as_deref()).as_deref(), plan.size, &fresh_ep_ids);
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
            db::log_activity(&conn, "grab", Some(plan.show_id), &msg);
            crate::applog::info("scheduler",format!("{msg}"));
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
        Err(e) => {
            let msg = format!("Failed to grab {} S{:02}: {e}", plan.show_name, plan.season);
            db::log_activity(&conn, "error", Some(plan.show_id), &msg);
            crate::applog::info("scheduler",format!("{msg}"));
            false
        }
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
fn reopen_dead_grabs(
    conn: &rusqlite::Connection,
    torrents: &[crate::qbit::QbitTorrent],
) -> Vec<DeadGrab> {
    let now = db::now();
    let mut dead = vec![];
    for t in torrents {
        // A live swarm serves metadata in seconds — 20 minutes of metaDL is a
        // ghost town. Stalled-with-data gets the longer benefit of the doubt.
        let dead_meta = t.state == "metaDL" && now - t.added_on > 20 * 60;
        let dead_stall = t.state == "stalledDL" && t.num_seeds == 0 && now - t.added_on > 3600;
        if !(dead_meta || dead_stall) {
            continue;
        }
        let norm = normalize(&t.name);
        // match by normalized title against still-open grabs
        let hit: Option<(i64, String, i64, Option<String>)> = conn
            .prepare("SELECT id, title, size, ep_ids FROM grab_ledger WHERE state = 'grabbed'")
            .ok()
            .and_then(|mut stmt| {
                stmt.query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get(3)?))
                })
                .ok()
                .and_then(|rows| rows.flatten().find(|(_, title, _, _)| normalize(title) == norm))
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
            db::set_episodes_state_by_ids(&conn, &linked_ids, "wanted", None);
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
                    "Dead swarm: {} had no peers for over an hour — paused; hunting a replacement",
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
    let q = crate::qbit::QbitClient {
        http: &state.http,
        base: cfg.qbit_url.clone(),
        username: cfg.qbit_username.clone(),
        password: cfg.qbit_password.clone(),
    };

    // only the ones we actually handle this cycle; the rest stay detectable
    for item in dead.into_iter().take(2) {
        // pause the dead torrent — recoverable, never deleted
        let _ = q.torrent_action("stop", &item.qbt_hash).await;

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
            std::time::Duration::from_secs(180),
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
                crate::applog::info("scheduler",format!("{msg}"));
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
            Err(_) => crate::applog::warn("scheduler",format!("medic run timed out")),
        }
    }
}

/// The btih out of a magnet link, lowercased — identity that survives
/// qBittorrent renaming the torrent once metadata arrives.
pub(crate) fn magnet_hash(magnet: Option<&str>) -> Option<String> {
    let m = magnet?;
    let idx = m.find("btih:")?;
    let rest = &m[idx + 5..];
    let end = rest.find('&').unwrap_or(rest.len());
    let h = &rest[..end];
    if h.len() == 40 || h.len() == 32 {
        Some(h.to_ascii_lowercase())
    } else {
        None
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
    let category = if cfg.qbit_category.is_empty() { None } else { Some(cfg.qbit_category.as_str()) };
    let torrents = match q.list(category).await {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    // swarm doctor: a session that reports "disconnected" has no working
    // sockets at all — on Windows the classic cause is the listen port
    // landing in a reserved range (WSAEACCES on every bind). Detect it and
    // fix it by moving the port, instead of letting every torrent sit at
    // "0 seeds" until someone SSHes in with a debugger.
    if let Ok(md) = q.sync_maindata().await {
        let status = md
            .pointer("/server_state/connection_status")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if status == "disconnected" {
            let new_port = crate::setup::pick_safe_listen_port();
            let body = format!("{{\"listen_port\": {new_port}}}");
            let moved = state
                .http
                .post(format!("{}/api/v2/app/setPreferences", cfg.qbit_url.trim_end_matches('/')))
                .form(&[("json", body)])
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if moved {
                crate::applog::warn(
                    "qbit",
                    format!("session was disconnected (no working sockets) — moved the listen port to {new_port}"),
                );
                let conn = state.db.lock().await;
                db::log_activity(
                    &conn,
                    "system",
                    None,
                    &format!("qBittorrent couldn't open its network port — Trawler moved it to {new_port} and the session should recover"),
                );
            } else {
                crate::applog::error("qbit", "session is disconnected and the port move failed — check Settings → Logs");
            }
        }
    }
    let mut done: std::collections::HashSet<String> = torrents
        .iter()
        .filter(|t| t.progress >= 0.999)
        .map(|t| normalize(&t.name))
        .collect();
    let done_hashes: std::collections::HashSet<String> = torrents
        .iter()
        .filter(|t| t.progress >= 0.999)
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
            conn.prepare("SELECT title, info_hash FROM grab_ledger WHERE info_hash IS NOT NULL")
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
    // Orphan reaping: a 'grabbed' ledger row whose torrent is nowhere in
    // qBittorrent means the user deleted it (in our Downloads view before
    // v0.3.7, or in qBittorrent itself). Holding the claim would block their
    // deliberate re-download forever — retire it and free the episodes.
    {
        let all_norms: std::collections::HashSet<String> =
            torrents.iter().map(|t| normalize(&t.name)).collect();
        let all_hashes: std::collections::HashSet<String> =
            torrents.iter().map(|t| t.hash.to_ascii_lowercase()).collect();
        let cutoff = db::now() - 5 * 60; // covers the add-to-listing gap; a magnet mid-metaDL is already listed
        let rows: Vec<(i64, String, Option<String>, Option<String>)> = conn
            .prepare("SELECT id, title, info_hash, ep_ids FROM grab_ledger WHERE state = 'grabbed' AND ts < ?1")
            .ok()
            .map(|mut stmt| {
                stmt.query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                    .map(|it| it.flatten().collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        for (id, title, info_hash, ep_ids_raw) in rows {
            let hash_present = info_hash
                .map(|h| all_hashes.contains(&h.to_ascii_lowercase()))
                .unwrap_or(false);
            if hash_present || all_norms.contains(&normalize(&title)) {
                continue;
            }
            let _ = conn.execute("UPDATE grab_ledger SET state = 'removed' WHERE id = ?1", [id]);
            let _ = conn.execute(
                "UPDATE episodes SET state = 'wanted', grabbed_title = NULL, grabbed_at = NULL,
                        last_searched_at = 0
                 WHERE state = 'grabbed' AND grabbed_title = ?1",
                [&title],
            );
            db::set_episodes_state_by_ids(&conn, &db::parse_ep_ids(ep_ids_raw.as_deref()), "wanted", None);
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
    let dead = reopen_dead_grabs(&conn, &torrents);
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
        torrents.iter().filter(|t| t.progress < 0.999).map(|t| normalize(&t.name)).collect();
    let running_hashes: std::collections::HashSet<String> =
        torrents.iter().filter(|t| t.progress < 0.999).map(|t| t.hash.to_ascii_lowercase()).collect();
    let open_ledger: Vec<(i64, String, i64, Option<String>, Option<String>)> = conn
        .prepare("SELECT id, title, ts, info_hash, ep_ids FROM grab_ledger WHERE state = 'grabbed'")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map(|it| it.flatten().collect::<Vec<_>>())
            .unwrap_or_default()
        })
        .unwrap_or_default();
    for (id, title, ts, info_hash, ep_ids_raw) in open_ledger {
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
        // resync: the torrent is still running but a refollow reset its
        // episodes to 'wanted' — put the truth back on the screen (and keep
        // the scheduler from grabbing a duplicate)
        let hash_running = h.as_ref().map(|x| running_hashes.contains(x)).unwrap_or(false);
        if (hash_running || running_norms.contains(&norm)) && !linked.is_empty() {
            db::set_episodes_state_by_ids(&conn, &linked, "grabbed", Some(&title));
        }
    }

    for item in &completed_display {
        db::log_activity(&conn, "complete", None, &format!("Finished downloading {item}"));
    }
    drop(conn);

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

    let now = db::now();
    let mut grabs = 0usize;
    // the follow path gets the same rails briefs have: a per-cycle grab cap
    // and a free-disk floor, so a catalog import can't flood the disk
    const MAX_GRABS_PER_CYCLE: usize = 6;
    let disk_ok = {
        let cfg = state.config.read().await.clone();
        let q = crate::qbit::QbitClient {
            http: &state.http,
            base: cfg.qbit_url.clone(),
            username: cfg.qbit_username.clone(),
            password: cfg.qbit_password.clone(),
        };
        match q.free_space().await {
            Ok(free) => (free as f64) >= cfg.agent_min_free_disk_gb.max(5.0) * 1e9,
            Err(_) => true, // can't tell — don't block grabs on a hiccup
        }
    };
    if !disk_ok {
        crate::applog::info("scheduler",format!("free disk below the floor — no grabs this cycle"));
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

        match plan_for_show(state, show, false).await {
            Ok(outcome) => {
                if !outcome.searched_ep_ids.is_empty() {
                    // stamp ONLY what was actually searched with indexers
                    // answering — stamping unsearched episodes starves them
                    let conn = state.db.lock().await;
                    db::stamp_searched(&conn, &outcome.searched_ep_ids);
                }
                for plan in &outcome.plans {
                    if !disk_ok || grabs >= MAX_GRABS_PER_CYCLE {
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
