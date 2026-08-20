//! The Upgrade Scout: an opt-in weekly pass over recently downloaded episodes
//! that looks for a meaningfully better release — higher resolution than what
//! was actually grabbed, within the show's quality profile. Strictly
//! propose-only: the scout never grabs; every find lands in the proposal
//! inbox with a clear before → after reason.

use tauri::Manager;

use crate::briefs::content_key_for_episode;
use crate::commands::perform_search;
use crate::config::QualityProfile;
use crate::db;
use crate::error::AppError;
use crate::follows::{codec_boost, episode_query, profile_allows};
use crate::AppState;

const PASS_INTERVAL_SECS: i64 = 7 * 86_400;
/// indexer courtesy: at most this many episode searches per weekly pass
const MAX_SEARCHES_PER_PASS: usize = 10;
/// a single show (e.g. one season pack) must not monopolize the pass
const MAX_PER_SHOW: usize = 2;
const META_KEY: &str = "upgrade_scout_last_pass";

pub fn res_rank(r: Option<&str>) -> i32 {
    match r {
        Some("2160p") => 4,
        Some("1080p") => 3,
        Some("720p") => 2,
        Some("480p") => 1,
        _ => 0,
    }
}

/// The resolution ceiling this profile explicitly asks for. An empty list
/// means "no preference" (config.rs: hard filter, empty = anything) — that is
/// NOT a mandate to chase 4K, so it creates no upgrade ceiling at all.
fn best_allowed_rank(profile: &QualityProfile) -> i32 {
    profile.resolutions.iter().map(|r| res_rank(Some(r))).max().unwrap_or(0)
}

struct Candidate {
    show_name: String,
    season: i64,
    number: i64,
    grabbed_title: String,
    grabbed_rank: i32,
    profile: QualityProfile,
    save_path: Option<String>,
}

pub async fn scout_loop(app: tauri::AppHandle) {
    // stay out of the way of startup, first scheduler cycle, first sweep
    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    loop {
        let (enabled, due) = {
            let state = app.state::<AppState>();
            let enabled = state.config.read().await.upgrade_scout_enabled;
            let due = {
                let conn = state.db.lock().await;
                let last: i64 = db::meta_get(&conn, META_KEY)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                db::now() - last >= PASS_INTERVAL_SECS
            };
            (enabled, due)
        };
        if enabled && due {
            match run_pass(&app).await {
                Ok(n) => {
                    let state = app.state::<AppState>();
                    let conn = state.db.lock().await;
                    db::meta_set(&conn, META_KEY, &db::now().to_string());
                    if n > 0 {
                        eprintln!("[trawler] upgrade scout: {n} proposal(s) filed");
                    }
                }
                // meta NOT stamped: the 6h tick retries instead of burning a week
                Err(e) => eprintln!("[trawler] upgrade scout pass failed: {e}"),
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
    }
}

/// Manual trigger (Settings): run one pass now; a completed pass resets the
/// weekly clock like a scheduled one.
pub async fn scan_now(app: &tauri::AppHandle) -> crate::error::Result<usize> {
    let n = run_pass(app).await?;
    let state = app.state::<AppState>();
    let conn = state.db.lock().await;
    db::meta_set(&conn, META_KEY, &db::now().to_string());
    Ok(n)
}

/// Has the user already dismissed an upgrade card for this content?
fn declined(conn: &rusqlite::Connection, ck: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM proposals WHERE content_key = ?1 AND status = 'dismissed'",
        [ck],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// Best resolution rank and largest size among everything already grabbed for
/// this content (the original grab AND any approved upgrade share the key).
fn already_have(conn: &rusqlite::Connection, ck: &str) -> (i32, i64) {
    let entries = db::ledger_entries(conn, ck);
    let rank = entries
        .iter()
        .map(|(t, _)| res_rank(crate::parse::parse(t).resolution.as_deref()))
        .max()
        .unwrap_or(0);
    let size = entries.iter().map(|(_, s)| *s).max().unwrap_or(0);
    (rank, size)
}

/// One weekly pass. Returns the number of NEW proposals filed.
async fn run_pass(app: &tauri::AppHandle) -> crate::error::Result<usize> {
    let state_guard = app.state::<AppState>();
    let state: &AppState = state_guard.inner();
    let cfg = state.config.read().await.clone();
    let window_secs = i64::from(cfg.upgrade_window_days.clamp(1, 365)) * 86_400;

    // Recently downloaded episodes whose grabbed release sits below the
    // profile ceiling — most recent first, hard-capped for indexer courtesy.
    let candidates: Vec<Candidate> = {
        let conn = state.db.lock().await;
        let mut stmt = match conn.prepare(
            "SELECT s.name, s.quality_json, s.save_path_override, e.season, e.number, e.grabbed_title
             FROM episodes e JOIN shows s ON s.tvmaze_id = e.show_id
             WHERE e.state = 'downloaded' AND e.grabbed_title IS NOT NULL
               AND e.grabbed_at > ?1
             ORDER BY e.grabbed_at DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Ok(0),
        };
        let rows: Vec<(String, Option<String>, Option<String>, i64, i64, String)> = stmt
            .query_map([db::now() - window_secs], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .map(|it| it.flatten().collect())
            .unwrap_or_default();
        let mut per_show: std::collections::HashMap<String, usize> = Default::default();
        rows.into_iter()
            .filter_map(|(name, quality_json, save_path_override, season, number, grabbed_title)| {
                let profile: QualityProfile = quality_json
                    .as_deref()
                    .and_then(|j| serde_json::from_str(j).ok())
                    .unwrap_or_else(|| cfg.default_quality.clone());
                let parsed = crate::parse::parse(&grabbed_title);
                let grabbed_rank = res_rank(parsed.resolution.as_deref());
                // unknown resolution: nothing to compare against — skip;
                // no explicit resolution list: no ceiling to upgrade toward
                if grabbed_rank == 0 || grabbed_rank >= best_allowed_rank(&profile) {
                    return None;
                }
                // pre-search guards, cheap: skip what the user declined and
                // what an approved upgrade already covers
                let ck = content_key_for_episode(&grabbed_title, season, number);
                if declined(&conn, &ck) {
                    return None;
                }
                let (have_rank, _) = already_have(&conn, &ck);
                if have_rank > grabbed_rank {
                    return None; // an approved upgrade is already in flight/done
                }
                let n = per_show.entry(name.clone()).or_insert(0);
                *n += 1;
                if *n > MAX_PER_SHOW {
                    return None; // a season pack must not eat the whole pass
                }
                let save_path = save_path_override
                    .filter(|p| !p.is_empty())
                    .or_else(|| {
                        if cfg.save_path_tv.is_empty() { None } else { Some(cfg.save_path_tv.clone()) }
                    });
                Some(Candidate {
                    show_name: name,
                    season,
                    number,
                    grabbed_title,
                    grabbed_rank,
                    profile,
                    save_path,
                })
            })
            .take(MAX_SEARCHES_PER_PASS)
            .collect()
    };
    if candidates.is_empty() {
        return Ok(0);
    }

    let mut new_proposals = 0usize;
    let mut searches_ok = 0usize;
    let mut searches_failed = 0usize;
    for c in &candidates {
        let q = episode_query(&c.show_name, c.season, c.number);
        let results = match perform_search(state, &q, "tv", &[]).await {
            Ok(r) => {
                searches_ok += 1;
                r
            }
            Err(_) => {
                searches_failed += 1;
                continue;
            }
        };
        // sanity ceiling: a candidate wildly larger than what we already have
        // is a remux trap, not an upgrade (mirrors the medic's heuristic)
        let ck_ep = content_key_for_episode(&c.grabbed_title, c.season, c.number);
        let orig_size = {
            let conn = state.db.lock().await;
            already_have(&conn, &ck_ep).1
        };
        let size_cap: i64 = if orig_size > 0 {
            (orig_size.saturating_mul(4)).max(3_000_000_000)
        } else {
            i64::MAX
        };
        let better = results
            .releases
            .iter()
            .filter(|r| r.relevant)
            .filter(|r| r.release.seeders.map(|s| s >= 3).unwrap_or(false))
            .filter(|r| r.parsed.season.map(i64::from) == Some(c.season))
            .filter(|r| r.parsed.episode.map(i64::from) == Some(c.number) && !r.parsed.season_pack)
            .filter(|r| res_rank(r.parsed.resolution.as_deref()) > c.grabbed_rank)
            .filter(|r| r.release.size <= size_cap)
            .filter(|r| profile_allows(&c.profile, &r.parsed, r.release.size, 1))
            .max_by(|a, b| {
                let sa = a.score + codec_boost(&c.profile, &a.parsed);
                let sb = b.score + codec_boost(&c.profile, &b.parsed);
                sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
            });

        if let Some(pick) = better {
            let old_res = crate::parse::parse(&c.grabbed_title)
                .resolution
                .unwrap_or_else(|| "unknown".into());
            let new_res = pick.parsed.resolution.clone().unwrap_or_else(|| "?".into());
            let reason = format!(
                "Upgrade: {} S{:02}E{:02} {} → {} — you already have a watchable copy; approving downloads the better one (the old file stays until you remove it)",
                c.show_name, c.season, c.number, old_res, new_res
            );
            let result_json = serde_json::to_string(&serde_json::json!({
                "title": pick.release.title,
                "size": pick.release.size,
                "seeders": pick.release.seeders,
                "indexer": pick.release.indexer,
                "resolution": pick.parsed.resolution,
                "source": pick.parsed.source,
                "codec": pick.parsed.codec,
                "magnetUrl": pick.release.magnet_url,
                "downloadUrl": pick.release.download_url,
                "infoHash": pick.release.info_hash,
                "savePath": c.save_path,
                "season": c.season,
                "episode": c.number,
            }))
            .unwrap_or_default();
            let ck = crate::briefs::content_key(&pick.release.title);
            let is_new = {
                let conn = state.db.lock().await;
                // release naming can normalize differently from the grabbed
                // title — re-run the guards on the pick's own key too
                let (have_rank, _) = already_have(&conn, &ck);
                if declined(&conn, &ck)
                    || res_rank(pick.parsed.resolution.as_deref()) <= have_rank
                {
                    false
                } else {
                    let is_new = db::proposal_upsert(&conn, None, &ck, &result_json, &reason);
                    if is_new {
                        db::log_activity(&conn, "agent", None, &reason);
                    }
                    is_new
                }
            };
            if is_new {
                new_proposals += 1;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    // every search failing is an infrastructure problem, not a completed pass
    if searches_ok == 0 && searches_failed > 0 {
        return Err(AppError::Other(
            "upgrade scout: every indexer search failed — will retry".into(),
        ));
    }

    {
        let conn = state.db.lock().await;
        db::log_activity(
            &conn,
            "agent",
            None,
            &format!(
                "Upgrade scout: checked {} recent download(s), filed {} proposal(s)",
                candidates.len(),
                new_proposals
            ),
        );
    }

    if new_proposals > 0 {
        crate::notify::dispatch(
            app,
            crate::notify::Kind::Proposal,
            format!(
                "Upgrade scout found {new_proposals} better release{}",
                if new_proposals == 1 { "" } else { "s" }
            ),
            "Waiting for your approval in the Agent view".into(),
        );
    }
    Ok(new_proposals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_resolutions() {
        assert!(res_rank(Some("2160p")) > res_rank(Some("1080p")));
        assert!(res_rank(Some("1080p")) > res_rank(Some("720p")));
        assert_eq!(res_rank(None), 0);
        assert_eq!(res_rank(Some("weird")), 0);
    }

    #[test]
    fn ceiling_from_profile() {
        let p = |res: &[&str]| QualityProfile {
            resolutions: res.iter().map(|s| s.to_string()).collect(),
            codec: "any".into(),
            max_size_gb: 0.0,
            allow_season_packs: true,
        };
        assert_eq!(best_allowed_rank(&p(&["1080p", "720p"])), 3);
        assert_eq!(best_allowed_rank(&p(&["2160p"])), 4);
        // empty = no preference = no upgrade ceiling (NOT "chase 4K")
        assert_eq!(best_allowed_rank(&p(&[])), 0);
    }

    #[test]
    fn episode_key_matches_release_key() {
        // an episode grabbed via season pack must map to the same content key
        // a single-episode release of it would produce
        let pack_ck = content_key_for_episode("Show.S01.COMPLETE.720p.WEB-DL.x265-GRP", 1, 5);
        let ep_ck = crate::briefs::content_key("Show.S01E05.1080p.WEB-DL.x265-OTHER");
        assert_eq!(pack_ck, ep_ck);
    }
}
