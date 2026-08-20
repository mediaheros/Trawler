//! The follow engine: TVmaze sync, episode bookkeeping, search-query building
//! and quality-profile filtering. The scheduler (slice 2 phase B) drives this.

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::config::QualityProfile;
use crate::db;
use crate::error::Result;
use crate::parse::ParsedRelease;
use crate::tvmaze::{self, TvmShow};
use crate::AppState;

// ---------- query building ----------

/// Indexers match release names, so strip punctuation that scene names drop.
/// TVmaze disambiguates same-name shows as "Show (2017)" / "Show (UK)" —
/// release groups don't, so those suffixes must not become required tokens.
fn strip_disambiguator(name: &str) -> String {
    match name.rfind(" (") {
        Some(i) if name.ends_with(')') => name[..i].to_string(),
        _ => name.to_string(),
    }
}

pub fn clean_show_name(name: &str) -> String {
    let name = &strip_disambiguator(name);
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '\'' | '\u{2019}' => {}                       // It's -> Its
            ':' | ',' | '(' | ')' | '.' | '!' | '?' => out.push(' '),
            '&' => out.push_str(" and "),
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn episode_query(show: &str, season: i64, episode: i64) -> String {
    format!("{} S{:02}E{:02}", clean_show_name(show), season, episode)
}

pub fn season_query(show: &str, season: i64) -> String {
    format!("{} S{:02}", clean_show_name(show), season)
}

// ---------- quality profile ----------

/// Hard filter: resolution list and per-episode size cap.
/// Unknown resolution passes (can't prove a violation), scoring sorts it out.
pub fn profile_allows(
    profile: &QualityProfile,
    parsed: &ParsedRelease,
    size: i64,
    episodes_covered: i64,
) -> bool {
    if !profile.resolutions.is_empty() {
        if let Some(res) = &parsed.resolution {
            if !profile.resolutions.iter().any(|r| r == res) {
                return false;
            }
        }
    }
    if profile.max_size_gb > 0.0 && size > 0 {
        let cap = profile.max_size_gb * 1e9 * episodes_covered.max(1) as f64;
        if size as f64 > cap {
            return false;
        }
    }
    true
}

/// Soft preference applied on top of the base score.
pub fn codec_boost(profile: &QualityProfile, parsed: &ParsedRelease) -> f64 {
    match (profile.codec.as_str(), parsed.codec.as_deref()) {
        ("prefer-x265", Some("x265")) => 8.0,
        ("prefer-x264", Some("x264")) => 8.0,
        _ => 0.0,
    }
}

// ---------- TVmaze sync ----------

fn aired(airstamp: &Option<String>) -> bool {
    airstamp
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t <= Utc::now())
        .unwrap_or(false)
}

/// Insert/update a show and its episodes. `seasons`: None = every season.
/// Existing episode states are preserved except upcoming→wanted when they air.
pub fn upsert_show(
    conn: &Connection,
    show: &TvmShow,
    backfill: bool,
    seasons: Option<&[i64]>,
    is_new_follow: bool,
) -> Result<()> {
    conn.execute(
        "INSERT INTO shows (tvmaze_id, name, status, poster_url, premiered, ended, network,
                            imdb_id, followed_at, refreshed_at, backfill)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)
         ON CONFLICT(tvmaze_id) DO UPDATE SET
           name = excluded.name, status = excluded.status, poster_url = excluded.poster_url,
           premiered = excluded.premiered, ended = excluded.ended, network = excluded.network,
           imdb_id = excluded.imdb_id, refreshed_at = excluded.refreshed_at",
        rusqlite::params![
            show.id,
            show.name,
            show.status,
            show.poster(),
            show.premiered,
            show.ended,
            show.network_name(),
            show.externals.as_ref().and_then(|e| e.imdb.clone()),
            db::now(),
            backfill as i64,
        ],
    )
    .map_err(db::db_err)?;

    let episodes = show
        .embedded
        .as_ref()
        .map(|e| e.episodes.as_slice())
        .unwrap_or(&[]);

    for ep in episodes {
        // regular numbered episodes only — specials don't fit SxxEyy matching
        let number = match ep.number {
            Some(n) => n,
            None => continue,
        };
        if matches!(ep.ep_type.as_deref(), Some(t) if t != "regular") {
            continue;
        }

        let season_selected = seasons.map(|s| s.contains(&ep.season)).unwrap_or(true);
        let has_aired = aired(&ep.airstamp);

        // State for NEW rows. New episodes appearing after the follow (a running
        // show's next season) are always wanted once they air.
        let initial_state = if !has_aired {
            "upcoming"
        } else if is_new_follow {
            if backfill && season_selected { "wanted" } else { "ignored" }
        } else {
            "wanted"
        };

        conn.execute(
            "INSERT INTO episodes (tvmaze_ep_id, show_id, season, number, title, airstamp, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(tvmaze_ep_id) DO UPDATE SET
               title = excluded.title, airstamp = excluded.airstamp,
               state = CASE
                 WHEN episodes.state = 'upcoming' AND excluded.state != 'upcoming' THEN 'wanted'
                 ELSE episodes.state
               END",
            rusqlite::params![
                ep.id,
                show.id,
                ep.season,
                number,
                ep.name,
                ep.airstamp,
                initial_state,
            ],
        )
        .map_err(db::db_err)?;
    }
    Ok(())
}

pub async fn follow(
    state: &AppState,
    tvmaze_id: i64,
    backfill: bool,
    seasons: Option<Vec<i64>>,
) -> Result<db::ShowRow> {
    let show = tvmaze::show_with_episodes(&state.http, tvmaze_id).await?;
    let conn = state.db.lock().await;
    upsert_show(&conn, &show, backfill, seasons.as_deref(), true)?;
    db::log_activity(
        &conn,
        "follow",
        Some(show.id),
        &format!("Following {} ({})", show.name, show.status),
    );
    let rows = db::list_shows(&conn)?;
    rows.into_iter()
        .find(|r| r.tvmaze_id == tvmaze_id)
        .ok_or_else(|| crate::error::AppError::Other("follow failed to persist".into()))
}

/// Re-sync one show from TVmaze (new episodes, status changes, airdate shifts).
pub async fn refresh_show(state: &AppState, tvmaze_id: i64) -> Result<()> {
    let show = tvmaze::show_with_episodes(&state.http, tvmaze_id).await?;
    let conn = state.db.lock().await;
    upsert_show(&conn, &show, true, None, false)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    #[test]
    fn query_building() {
        assert_eq!(episode_query("It's Always Sunny!", 16, 3), "Its Always Sunny S16E03");
        assert_eq!(season_query("Dune: Prophecy", 1), "Dune Prophecy S01");
        assert_eq!(clean_show_name("Mr. & Mrs. Smith"), "Mr and Mrs Smith");
    }

    #[test]
    fn profile_filtering() {
        let profile = QualityProfile {
            resolutions: vec!["1080p".into(), "720p".into()],
            codec: "prefer-x265".into(),
            max_size_gb: 4.0,
            allow_season_packs: true,
        };
        let hi = parse::parse("Show.S01E01.2160p.WEB-DL.HEVC");
        let ok = parse::parse("Show.S01E01.1080p.WEB-DL.x265");
        let unknown = parse::parse("Show.S01E01.WEB-DL");
        assert!(!profile_allows(&profile, &hi, 2_000_000_000, 1));
        assert!(profile_allows(&profile, &ok, 2_000_000_000, 1));
        assert!(profile_allows(&profile, &unknown, 2_000_000_000, 1));
        // size cap: 5GB single episode rejected, but fine spread over a 10-episode pack
        assert!(!profile_allows(&profile, &ok, 5_000_000_000, 1));
        assert!(profile_allows(&profile, &ok, 5_000_000_000, 10));
        assert_eq!(codec_boost(&profile, &ok), 8.0);
        assert_eq!(codec_boost(&profile, &unknown), 0.0);
    }
}
