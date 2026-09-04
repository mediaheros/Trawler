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
    // The season selection must survive refreshes — NULL means "all seasons"
    // and never overwrites a stored selection (refreshes pass None).
    let seasons_json = seasons.map(|s| serde_json::to_string(s).unwrap_or_else(|_| "null".into()));
    conn.execute(
        "INSERT INTO shows (tvmaze_id, name, status, poster_url, premiered, ended, network,
                            imdb_id, followed_at, refreshed_at, backfill, seasons_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?11)
         ON CONFLICT(tvmaze_id) DO UPDATE SET
           name = excluded.name, status = excluded.status, poster_url = excluded.poster_url,
           premiered = excluded.premiered, ended = excluded.ended, network = excluded.network,
           imdb_id = excluded.imdb_id, refreshed_at = excluded.refreshed_at,
           seasons_json = COALESCE(excluded.seasons_json, shows.seasons_json)",
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
            seasons_json,
        ],
    )
    .map_err(db::db_err)?;

    let episodes = show
        .embedded
        .as_ref()
        .map(|e| e.episodes.as_slice())
        .unwrap_or(&[]);

    // one transaction: a 500-episode show was 500 WAL fsyncs while every UI
    // command and the RSS sweep waited on the connection lock
    let tx = conn.unchecked_transaction().map_err(db::db_err)?;
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

        // State for NEW rows. The season selection is the gate: episodes of
        // an unselected season never become wanted, whether they aired
        // before the follow or air after it.
        let initial_state = if !season_selected {
            "ignored"
        } else if !has_aired {
            "upcoming"
        } else if is_new_follow {
            if backfill { "wanted" } else { "ignored" }
        } else {
            "wanted"
        };

        tx.execute(
            "INSERT INTO episodes (tvmaze_ep_id, show_id, season, number, title, airstamp, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(tvmaze_ep_id) DO UPDATE SET
               title = excluded.title, airstamp = excluded.airstamp,
               state = CASE
                 WHEN episodes.state = 'upcoming' AND excluded.state = 'wanted' THEN 'wanted'
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
    tx.commit().map_err(db::db_err)?;
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
    // honor the stored season selection — a refresh must never widen what
    // the user chose to follow
    let seasons: Option<Vec<i64>> = conn
        .query_row(
            "SELECT seasons_json FROM shows WHERE tvmaze_id = ?1",
            [tvmaze_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok());
    upsert_show(&conn, &show, true, seasons.as_deref(), false)?;
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

    // ---- season-selection gating tests ----

    fn mem() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE shows (
                 tvmaze_id INTEGER PRIMARY KEY, name TEXT NOT NULL, status TEXT NOT NULL,
                 poster_url TEXT, premiered TEXT, ended TEXT, network TEXT, imdb_id TEXT,
                 followed_at INTEGER NOT NULL, refreshed_at INTEGER NOT NULL DEFAULT 0,
                 backfill INTEGER, seasons_json TEXT
               );
               CREATE TABLE episodes (
                 tvmaze_ep_id INTEGER PRIMARY KEY, show_id INTEGER NOT NULL,
                 season INTEGER NOT NULL, number INTEGER NOT NULL,
                 title TEXT, airstamp TEXT, state TEXT NOT NULL DEFAULT 'wanted'
               );",
        )
        .unwrap();
        conn
    }

    fn mk_show(eps: &[(i64, i64, Option<i64>, &str)]) -> crate::tvmaze::TvmShow {
        crate::tvmaze::TvmShow {
            id: 1,
            name: "Test Show".into(),
            status: "Running".into(),
            premiered: None, ended: None, genres: vec![],
            network: None, web_channel: None, image: None, externals: None,
            summary: None,
            embedded: Some(crate::tvmaze::TvmEmbedded {
                episodes: eps.iter().map(|&(id, season, number, airstamp)| crate::tvmaze::TvmEpisode {
                    id, season, number, name: None,
                    airstamp: Some(airstamp.into()), ep_type: Some("regular".into()),
                }).collect(),
            }),
        }
    }

    fn states(conn: &rusqlite::Connection) -> Vec<(i64, String)> {
        conn.prepare("SELECT tvmaze_ep_id, state FROM episodes ORDER BY tvmaze_ep_id").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
            .flatten().collect()
    }

    #[test]
    fn season_selection_gates_new_follow_rows() {
        let conn = mem();
        let past = "2020-01-01T00:00:00+00:00";
        let future = "2999-01-01T00:00:00+00:00";
        let show = mk_show(&[
            (10, 1, Some(1), past),   // S1 aired
            (11, 1, Some(2), future), // S1 unaired
            (12, 2, Some(1), past),   // S2 aired
            (13, 2, Some(2), future), // S2 unaired
        ]);
        upsert_show(&conn, &show, true, Some(&[1]), true).unwrap();
        assert_eq!(states(&conn), vec![
            (10, "wanted".to_string()),
            (11, "upcoming".to_string()),
            (12, "ignored".to_string()),
            (13, "ignored".to_string()),
        ], "unselected seasons are ignored whether aired or not");

        // the selection persists
        let stored: Option<String> = conn.query_row(
            "SELECT seasons_json FROM shows WHERE tvmaze_id = ?1", [1], |r| r.get(0),
        ).unwrap();
        assert_eq!(stored.as_deref(), Some("[1]"));
    }

    #[test]
    fn refresh_promotes_only_selected_seasons() {
        let conn = mem();
        let future = "2999-01-01T00:00:00+00:00";
        let now_aired = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
        let show = mk_show(&[
            (10, 1, Some(1), future),     // S1, still future at follow time
            (11, 2, Some(1), &now_aired), // S2, aired
        ]);
        upsert_show(&conn, &show, true, Some(&[1]), true).unwrap();
        // time passes; refresh with the SAME episodes, S1 now aired too
        let show2 = mk_show(&[
            (10, 1, Some(1), &now_aired),
            (11, 2, Some(1), &now_aired),
        ]);
        upsert_show(&conn, &show2, true, Some(&[1]), false).unwrap();
        let mut s = states(&conn);
        s.sort_by_key(|(id, _)| *id);
        assert_eq!(s, vec![
            (10, "wanted".to_string()),
            (11, "ignored".to_string()),
        ], "selected season promotes on refresh; unselected stays ignored");
    }
}
