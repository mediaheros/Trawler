//! RSS-style sync: instead of searching per wanted item, pull each indexer's
//! LATEST releases every few minutes and match that stream against everything
//! Trawler wants — followed shows' episodes and standing briefs. This is how
//! new content gets grabbed within minutes of hitting an indexer, at a cost of
//! one query per indexer per sweep regardless of library size.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use tauri::{AppHandle, Manager};

use crate::briefs::{content_key, BriefRow, HuntPlan};
use crate::commands::{normalize, score};
use crate::config::QualityProfile;
use crate::db;
use crate::error::Result;
use crate::parse::{self, ParsedRelease};
use crate::prowlarr::{ProwlarrClient, ProwlarrRelease};
use crate::AppState;

/// A followed show's open wants, prepared for fast matching.
struct ShowWants {
    show_id: i64,
    show_name: String,
    /// normalized whole-word tokens that must all appear in a matching title
    name_tokens: Vec<String>,
    /// (season, episode) -> tvmaze_ep_id, for episodes in `wanted` state
    wanted: HashMap<(i32, i32), i64>,
    profile: QualityProfile,
    save_path_override: Option<String>,
}

fn name_tokens(name: &str) -> Vec<String> {
    normalize(&crate::follows::clean_show_name(name))
        .split(' ')
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// Whole-token match: every show token must appear as a complete word in the
/// title. Substrings don't count — "Us" must never match "hoUSe".
fn title_has_all(title_tokens: &HashSet<&str>, tokens: &[String]) -> bool {
    !tokens.is_empty() && tokens.iter().all(|t| title_tokens.contains(t.as_str()))
}

/// Newly aired episodes must become wanted the moment they air, not on the
/// next 20-hourly metadata refresh. Airstamps are RFC3339 with +00:00 offsets
/// (TVmaze convention), so lexicographic comparison against a UTC now works.
/// A show followed with a season selection only promotes ITS seasons — the
/// whole point of "Season 1 only" is not grabbing Season 2 later.
pub fn promote_aired(conn: &Connection) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    // restricted follows: show id -> the only seasons allowed to promote
    let restricted: Vec<(i64, Vec<i64>)> = conn
        .prepare("SELECT tvmaze_id, seasons_json FROM shows WHERE seasons_json IS NOT NULL")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .map(|it| {
                    it.flatten()
                        .filter_map(|(id, json)| {
                            serde_json::from_str::<Vec<i64>>(&json).ok().map(|s| (id, s))
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();

    if restricted.is_empty() {
        let _ = conn.execute(
            "UPDATE episodes SET state = 'wanted'
             WHERE state = 'upcoming' AND airstamp IS NOT NULL AND airstamp <= ?1",
            [&now],
        );
        return;
    }

    // unrestricted shows promote wholesale
    let ids: Vec<String> = restricted.iter().map(|(id, _)| id.to_string()).collect();
    let _ = conn.execute(
        &format!(
            "UPDATE episodes SET state = 'wanted'
             WHERE state = 'upcoming' AND airstamp IS NOT NULL AND airstamp <= ?1
               AND show_id NOT IN ({})",
            ids.join(",")
        ),
        [&now],
    );
    // restricted shows promote only their selected seasons
    for (show_id, seasons) in &restricted {
        if seasons.is_empty() {
            continue;
        }
        let placeholders = seasons.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut params: Vec<rusqlite::types::Value> =
            vec![now.clone().into(), (*show_id).into()];
        params.extend(seasons.iter().map(|s| rusqlite::types::Value::from(*s)));
        let _ = conn.execute(
            &format!(
                "UPDATE episodes SET state = 'wanted'
                 WHERE state = 'upcoming' AND airstamp IS NOT NULL AND airstamp <= ?1
                   AND show_id = ?2 AND season IN ({placeholders})"
            ),
            rusqlite::params_from_iter(params),
        );
    }
}

fn load_show_wants(conn: &Connection, default_profile: &QualityProfile) -> Vec<ShowWants> {
    let shows = match db::list_shows(conn) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    shows
        .iter()
        .filter(|s| s.wanted > 0)
        .map(|s| {
            let profile = s
                .quality_json
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok())
                .unwrap_or_else(|| default_profile.clone());
            let wanted = db::list_episodes(conn, s.tvmaze_id)
                .unwrap_or_default()
                .iter()
                .filter(|e| e.state == "wanted")
                .map(|e| ((e.season as i32, e.number as i32), e.tvmaze_ep_id))
                .collect();
            let alias = s.search_alias.clone().filter(|a| !a.is_empty());
            ShowWants {
                show_id: s.tvmaze_id,
                show_name: s.name.clone(),
                name_tokens: name_tokens(alias.as_deref().unwrap_or(&s.name)),
                wanted,
                profile,
                save_path_override: s.save_path_override.clone().filter(|p| !p.is_empty()),
            }
        })
        .collect()
}

/// A matched candidate waiting for best-of-sweep selection.
struct Candidate {
    release: ProwlarrRelease,
    parsed: ParsedRelease,
    score: f64,
}

pub struct SweepStats {
    pub releases_seen: usize,
    pub episode_grabs: usize,
    pub brief_grabs: usize,
    pub brief_proposals: usize,
    /// another sweep was already running
    pub skipped: bool,
}

impl SweepStats {
    fn empty(skipped: bool) -> Self {
        Self { releases_seen: 0, episode_grabs: 0, brief_grabs: 0, brief_proposals: 0, skipped }
    }
}

/// Clears the busy flag even on early return or error.
struct BusyGuard<'a>(&'a std::sync::atomic::AtomicBool);
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// One sweep: fetch latest from every enabled indexer, match, act.
pub async fn sweep(app: &AppHandle) -> Result<SweepStats> {
    let state_guard = app.state::<AppState>();
    let state: &AppState = state_guard.inner();

    if state.rss_busy.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Ok(SweepStats::empty(true));
    }
    let _busy = BusyGuard(&state.rss_busy);

    let cfg = state.config.read().await.clone();
    if cfg.prowlarr_api_key.is_empty() {
        return Ok(SweepStats::empty(false));
    }
    let client = ProwlarrClient {
        http: &state.http,
        base: cfg.prowlarr_url.clone(),
        api_key: cfg.prowlarr_api_key.clone(),
    };

    // What do we want right now? (promote freshly-aired episodes first)
    let (show_wants, briefs) = {
        let conn = state.db.lock().await;
        promote_aired(&conn);
        let wants = load_show_wants(&conn, &cfg.default_quality);
        // The brief arm is agent functionality: honor the agent kill-switch, and
        // only run briefs whose compiled plan is discriminating — a plan with no
        // required terms would treat the raw firehose as all-you-can-grab.
        let briefs: Vec<(BriefRow, HuntPlan)> = if cfg.agent_enabled {
            crate::briefs::list(&conn)
                .unwrap_or_default()
                .into_iter()
                .filter(|b| b.enabled && b.fail_streak < 3)
                .filter_map(|b| {
                    let plan: HuntPlan = serde_json::from_str(&b.plan_json).ok()?;
                    if plan.include.is_empty() {
                        return None;
                    }
                    Some((b, plan))
                })
                .collect()
        } else {
            vec![]
        };
        (wants, briefs)
    };
    if show_wants.is_empty() && briefs.is_empty() {
        return Ok(SweepStats::empty(false));
    }

    // Latest releases from every enabled indexer, in parallel with deadlines.
    let indexers: Vec<i32> = client
        .indexers()
        .await?
        .into_iter()
        .filter(|i| i.enable)
        .map(|i| i.id)
        .collect();
    let client_ref = &client;
    let fetches = indexers.iter().map(|id| async move {
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            client_ref.search("", &[], &[*id], 100),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default()
    });
    let all: Vec<ProwlarrRelease> = futures::future::join_all(fetches).await.into_iter().flatten().collect();
    let releases_seen = all.len();

    // ---- match: collect best candidate per wanted episode / per brief content ----
    let mut ep_candidates: HashMap<(i64, i64), Candidate> = HashMap::new();
    let mut brief_candidates: HashMap<(i64, String), Candidate> = HashMap::new();

    for r in all {
        if r.seeders.map(|s| s < 1).unwrap_or(false) {
            continue;
        }
        let parsed = parse::parse(&r.title);
        let base_score = score(&r, &parsed);
        let norm_title = normalize(&r.title);
        let title_tokens: HashSet<&str> = norm_title.split(' ').filter(|t| !t.is_empty()).collect();

        if let (Some(season), Some(episode)) = (parsed.season, parsed.episode) {
            // Most-specific show wins: "Vikings Valhalla S03E01" must bind to
            // Vikings: Valhalla, not to Vikings. Ties between distinct shows
            // are ambiguous — skip rather than guess.
            let mut matches: Vec<&ShowWants> = show_wants
                .iter()
                .filter(|w| w.wanted.contains_key(&(season, episode)))
                .filter(|w| title_has_all(&title_tokens, &w.name_tokens))
                .filter(|w| crate::follows::profile_allows(&w.profile, &parsed, r.size, 1))
                .collect();
            matches.sort_by_key(|w| std::cmp::Reverse(w.name_tokens.len()));
            let unambiguous = match matches.as_slice() {
                [only] => Some(*only),
                [first, second, ..] if first.name_tokens.len() > second.name_tokens.len() => Some(*first),
                _ => None,
            };
            if let Some(w) = unambiguous {
                let ep_id = w.wanted[&(season, episode)];
                let sc = base_score + crate::follows::codec_boost(&w.profile, &parsed);
                let key = (w.show_id, ep_id);
                let better = ep_candidates.get(&key).map(|c| sc > c.score).unwrap_or(true);
                if better {
                    ep_candidates.insert(key, Candidate { release: r.clone(), parsed: parsed.clone(), score: sc });
                }
            }
        }

        for (b, plan) in &briefs {
            if plan.allows(&r.title, r.size, r.seeders).is_ok() {
                let ck = content_key(&r.title);
                let key = (b.id, ck);
                let better = brief_candidates.get(&key).map(|c| base_score > c.score).unwrap_or(true);
                if better {
                    brief_candidates.insert(key, Candidate { release: r.clone(), parsed: parsed.clone(), score: base_score });
                }
            }
        }
    }

    // free-disk floor, checked once per sweep (best effort)
    let free_disk_ok = {
        let q = crate::qbit::QbitClient {
            http: &state.http,
            base: cfg.qbit_url.clone(),
            username: cfg.qbit_username.clone(),
            password: cfg.qbit_password.clone(),
        };
        match q.free_space().await {
            Ok(free) => (free as f64) >= cfg.agent_min_free_disk_gb * 1e9,
            Err(_) => true,
        }
    };

    // ---- act on episode matches ----
    let mut episode_grabs = 0usize;
    // same discipline as the scheduler cycle: a freshly imported catalog of
    // wanted episodes must not turn one sweep into dozens of grabs onto a
    // nearly-full disk
    const MAX_EPISODE_GRABS_PER_SWEEP: usize = 8;
    for ((show_id, ep_id), cand) in ep_candidates {
        if episode_grabs >= MAX_EPISODE_GRABS_PER_SWEEP {
            break;
        }
        if !free_disk_ok {
            break;
        }
        let ck = content_key(&cand.release.title);
        // re-check freshness under the lock: another path may have satisfied
        // this episode while we were fetching
        let (still_wanted, w_save_path, show_name) = {
            let conn = state.db.lock().await;
            if db::ledger_satisfied(&conn, &ck) {
                continue;
            }
            let still: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM episodes WHERE tvmaze_ep_id = ?1 AND state = 'wanted'",
                    [ep_id],
                    |r| r.get::<_, i64>(0),
                )
                .map(|n| n > 0)
                .unwrap_or(false);
            let w = show_wants.iter().find(|w| w.show_id == show_id);
            (
                still,
                w.and_then(|w| w.save_path_override.clone()),
                w.map(|w| w.show_name.clone()).unwrap_or_default(),
            )
        };
        if !still_wanted {
            continue;
        }
        let save_path = w_save_path.or_else(|| {
            if cfg.save_path_tv.is_empty() { None } else { Some(cfg.save_path_tv.clone()) }
        });
        let outcome = crate::grab::dispatch(
            state,
            crate::grab::GrabOrder {
                title: cand.release.title.clone(),
                magnet_url: cand.release.magnet_url.clone(),
                download_url: cand.release.download_url.clone(),
                save_path,
                info_hash: cand.release.info_hash.clone(),
                size: cand.release.size,
            },
            None,
            vec![ep_id],
        )
        .await;
        match outcome {
            Ok(crate::grab::GrabOutcome::Grabbed { .. }) => {
                episode_grabs += 1;
                {
                    let conn = state.db.lock().await;
                    db::log_activity(
                        &conn,
                        "rss",
                        Some(show_id),
                        &format!(
                            "RSS grabbed {} S{:02}E{:02} minutes after release · {}",
                            show_name,
                            cand.parsed.season.unwrap_or(0),
                            cand.parsed.episode.unwrap_or(0),
                            cand.release.title.chars().take(60).collect::<String>()
                        ),
                    );
                }
                crate::notify::dispatch(
                    app,
                    crate::notify::Kind::Grab,
                    format!(
                        "Grabbed {} S{:02}E{:02}",
                        show_name,
                        cand.parsed.season.unwrap_or(0),
                        cand.parsed.episode.unwrap_or(0)
                    ),
                    format!(
                        "Minutes after release · {:.1} GB · {}",
                        cand.release.size as f64 / 1e9,
                        cand.release.title.chars().take(90).collect::<String>()
                    ),
                );
            }
            // satisfied or claimed by another path — the ledger covers it
            Ok(_) => {}
            Err(e) => {
                let conn = state.db.lock().await;
                db::log_activity(&conn, "error", Some(show_id), &format!("RSS grab failed: {e}"));
            }
        }
    }

    // ---- act on brief matches (Rust-gated; the compiled plan is the law) ----
    let mut brief_grabs = 0usize;
    let mut brief_proposals = 0usize;
    let mut grabs_this_sweep: HashMap<i64, i64> = HashMap::new();
    let mut gb_this_sweep: HashMap<i64, f64> = HashMap::new();
    for ((brief_id, ck), cand) in brief_candidates {
        let Some((brief, _plan)) = briefs.iter().find(|(b, _)| b.id == brief_id) else { continue };
        {
            let conn = state.db.lock().await;
            if db::ledger_satisfied(&conn, &ck) {
                continue;
            }
        }
        if brief.mode == "auto" {
            if !free_disk_ok {
                continue;
            }
            let size_gb = cand.release.size as f64 / 1e9;
            let used = grabs_this_sweep.entry(brief_id).or_insert(0);
            let used_gb = gb_this_sweep.entry(brief_id).or_insert(0.0);
            if *used >= brief.max_grabs_per_run || *used_gb + size_gb > brief.max_gb_per_run {
                continue;
            }
            // rolling daily budgets + the same anomaly ceiling the runner uses
            let daily_ok = {
                let conn = state.db.lock().await;
                let ceiling = (brief.max_grabs_per_run * 4).max(8);
                db::ledger_gb_today(&conn, brief_id) + size_gb <= brief.max_gb_per_day
                    && db::ledger_grabs_today(&conn, brief_id) < ceiling
            };
            if !daily_ok {
                continue;
            }
            let outcome = crate::grab::dispatch(
                state,
                crate::grab::GrabOrder {
                    title: cand.release.title.clone(),
                    magnet_url: cand.release.magnet_url.clone(),
                    download_url: cand.release.download_url.clone(),
                    save_path: None,
                    info_hash: cand.release.info_hash.clone(),
                    size: cand.release.size,
                },
                Some(brief_id),
                vec![],
            )
            .await;
            match outcome {
                Ok(crate::grab::GrabOutcome::Grabbed { .. }) => {
                    *used += 1;
                    *gb_this_sweep.get_mut(&brief_id).unwrap() += size_gb;
                    brief_grabs += 1;
                    {
                        let conn = state.db.lock().await;
                        db::log_activity(
                            &conn,
                            "rss",
                            None,
                            &format!("[brief: {}] RSS grabbed {}", brief.name, cand.release.title.chars().take(60).collect::<String>()),
                        );
                    }
                    if cfg.notify_on_grab {
                        use tauri_plugin_notification::NotificationExt;
                        let _ = app.notification().builder().title(format!("Trawler brief: {}", brief.name)).body("Grabbed a fresh release").show();
                    }
                    crate::notify::dispatch(
                        app,
                        crate::notify::Kind::Grab,
                        format!("Brief \u{201C}{}\u{201D} grabbed a fresh release", brief.name),
                        format!(
                            "{:.1} GB · {}",
                            cand.release.size as f64 / 1e9,
                            cand.release.title.chars().take(90).collect::<String>()
                        ),
                    );
                }
                // satisfied or claimed by another path — nothing to bill
                Ok(_) => {}
                Err(e) => {
                    let conn = state.db.lock().await;
                    db::log_activity(&conn, "error", None, &format!("[brief: {}] RSS grab failed: {e}", brief.name));
                }
            }
        } else {
            let conn = state.db.lock().await;
            let result_json = serde_json::to_string(&serde_json::json!({
                "title": cand.release.title,
                "size": cand.release.size,
                "seeders": cand.release.seeders,
                "indexer": cand.release.indexer,
                "resolution": cand.parsed.resolution,
                "source": cand.parsed.source,
                "codec": cand.parsed.codec,
                "magnetUrl": cand.release.magnet_url,
                "downloadUrl": cand.release.download_url,
                "infoHash": cand.release.info_hash,
            }))
            .unwrap_or_default();
            let is_new = db::proposal_upsert(&conn, Some(brief_id), &ck, &result_json, "Matched your brief within minutes of release (RSS)");
            brief_proposals += 1;
            if is_new {
                crate::notify::dispatch(
                    app,
                    crate::notify::Kind::Proposal,
                    format!("Brief \u{201C}{}\u{201D} found a match", brief.name),
                    format!(
                        "{} · waiting for your approval in the Agent view",
                        cand.release.title.chars().take(90).collect::<String>()
                    ),
                );
            }
        }
    }

    if episode_grabs + brief_grabs + brief_proposals > 0 {
        crate::applog::info(
            "rss",
            format!("rss sweep: {releases_seen} releases → {episode_grabs} episode grabs, {brief_grabs} brief grabs, {brief_proposals} proposals"),
        );
    }
    Ok(SweepStats { releases_seen, episode_grabs, brief_grabs, brief_proposals, skipped: false })
}

pub async fn rss_loop(app: AppHandle) {
    tokio::time::sleep(std::time::Duration::from_secs(90)).await;
    loop {
        let (enabled, minutes) = {
            let state = app.state::<AppState>();
            let cfg = state.config.read().await;
            (cfg.rss_enabled, cfg.rss_minutes.clamp(10, 120) as u64)
        };
        if enabled {
            if let Err(e) = sweep(&app).await {
                crate::applog::warn("rss",format!("rss sweep failed: {e}"));
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(minutes * 60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal schema mirroring the columns promote_aired touches.
    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE shows (tvmaze_id INTEGER PRIMARY KEY, seasons_json TEXT);
             CREATE TABLE episodes (
               tvmaze_ep_id INTEGER PRIMARY KEY, show_id INTEGER NOT NULL,
               season INTEGER NOT NULL, number INTEGER NOT NULL,
               title TEXT, airstamp TEXT, state TEXT NOT NULL DEFAULT 'wanted'
             );",
        )
        .unwrap();
        conn
    }

    fn state_of(conn: &Connection, ep: i64) -> String {
        conn.query_row("SELECT state FROM episodes WHERE tvmaze_ep_id = ?1", [ep], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn promote_aired_flips_aired_upcoming_for_unrestricted_shows() {
        let conn = mem();
        conn.execute("INSERT INTO shows (tvmaze_id) VALUES (1)", []).unwrap();
        let past = "2020-01-01T00:00:00+00:00";
        let future = "2999-01-01T00:00:00+00:00";
        conn.execute(
            "INSERT INTO episodes (tvmaze_ep_id, show_id, season, number, airstamp, state) VALUES
             (10, 1, 1, 1, ?1, 'upcoming'), (11, 1, 1, 2, ?2, 'upcoming')",
            rusqlite::params![past, future],
        )
        .unwrap();
        promote_aired(&conn);
        assert_eq!(state_of(&conn, 10), "wanted", "aired upcoming promotes");
        assert_eq!(state_of(&conn, 11), "upcoming", "unaired stays upcoming");
    }

    #[test]
    fn promote_aired_respects_season_selections() {
        let conn = mem();
        // show 1 followed with "Season 1 only"; show 2 unrestricted
        conn.execute(
            "INSERT INTO shows (tvmaze_id, seasons_json) VALUES (1, '[1]'), (2, NULL)",
            [],
        )
        .unwrap();
        let past = "2020-01-01T00:00:00+00:00";
        conn.execute(
            "INSERT INTO episodes (tvmaze_ep_id, show_id, season, number, airstamp, state) VALUES
             (10, 1, 1, 5, ?1, 'upcoming'),
             (11, 1, 2, 1, ?1, 'upcoming'),
             (12, 2, 3, 1, ?1, 'upcoming')",
            [past],
        )
        .unwrap();
        promote_aired(&conn);
        assert_eq!(state_of(&conn, 10), "wanted", "selected season promotes");
        assert_eq!(
            state_of(&conn, 11),
            "upcoming",
            "unselected season must never promote — that's the whole point of the selection"
        );
        assert_eq!(state_of(&conn, 12), "wanted", "unrestricted show unaffected");
    }

    fn tokset(s: &str) -> String {
        normalize(s)
    }

    fn tokens_of(norm: &str) -> HashSet<&str> {
        norm.split(' ').filter(|t| !t.is_empty()).collect()
    }

    #[test]
    fn whole_token_matching_only() {
        let us = name_tokens("Us");
        let house_title = tokset("House.S01E02.1080p.WEB-DL.x265-GROUP");
        // "us" is a substring of "house" but NOT a whole token — must not match
        assert!(!title_has_all(&tokens_of(&house_title), &us));
        let us_title = tokset("Us.S01E02.1080p.WEB-DL");
        assert!(title_has_all(&tokens_of(&us_title), &us));
    }

    #[test]
    fn tokens_and_matching() {
        let tokens = name_tokens("The Expanse");
        let t1 = tokset("The.Expanse.S02E05.1080p.WEB");
        assert!(title_has_all(&tokens_of(&t1), &tokens));
        let t2 = tokset("Expanse.Documentary.2020");
        assert!(!title_has_all(&tokens_of(&t2), &tokens));
        assert!(!title_has_all(&tokens_of("anything"), &[]));
    }

    #[test]
    fn show_name_with_punctuation() {
        let tokens = name_tokens("Mr. & Mrs. Smith");
        let t = tokset("Mr.and.Mrs.Smith.S01E03.2160p.WEB.h265");
        assert!(title_has_all(&tokens_of(&t), &tokens));
    }
}
