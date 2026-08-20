//! Calendar, iCal export, and TVmaze-powered discovery.

use serde::Serialize;
use serde_json::{json, Value};
use tauri::State;

use crate::db;
use crate::error::{AppError, Result};
use crate::AppState;

// ---------------- calendar ----------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarItem {
    pub show_id: i64,
    pub show_name: String,
    pub poster_url: Option<String>,
    pub season: i64,
    pub number: i64,
    pub title: Option<String>,
    pub airstamp: String,
    /// the LOCAL calendar day this airs on (YYYY-MM-DD) — the UI buckets by
    /// this so a 00:30 UTC airstamp lands on the right local day
    pub local_date: String,
    pub state: String,
}

/// Episodes of followed shows whose airstamp falls in [start, end) (ISO dates).
#[tauri::command]
pub async fn calendar_range(
    state: State<'_, AppState>,
    start: String,
    end: String,
) -> Result<Vec<CalendarItem>> {
    let conn = state.db.lock().await;
    let mut stmt = conn
        .prepare(
            "SELECT e.show_id, s.name, s.poster_url, e.season, e.number, e.title, e.airstamp, e.state
             FROM episodes e JOIN shows s ON s.tvmaze_id = e.show_id
             WHERE e.airstamp IS NOT NULL AND e.airstamp >= ?1 AND e.airstamp < ?2
             ORDER BY e.airstamp",
        )
        .map_err(db::db_err)?;
    let rows = stmt
        .query_map(rusqlite::params![start, end], |r| {
            let airstamp: String = r.get(6)?;
            let local_date = chrono::DateTime::parse_from_rfc3339(&airstamp)
                .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
                .unwrap_or_else(|_| airstamp.chars().take(10).collect());
            Ok(CalendarItem {
                show_id: r.get(0)?,
                show_name: r.get(1)?,
                poster_url: r.get(2)?,
                season: r.get(3)?,
                number: r.get(4)?,
                title: r.get(5)?,
                airstamp,
                local_date,
                state: r.get(7)?,
            })
        })
        .map_err(db::db_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db::db_err)?;
    Ok(rows)
}

// ---------------- iCal export ----------------

fn ics_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace('\n', "\\n")
}

pub fn build_ics(items: &[CalendarItem]) -> String {
    let mut out = String::from(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Hero Media//Trawler//EN\r\nCALSCALE:GREGORIAN\r\nX-WR-CALNAME:Trawler — upcoming episodes\r\n",
    );
    for it in items {
        let Ok(t) = chrono::DateTime::parse_from_rfc3339(&it.airstamp) else { continue };
        let utc = t.with_timezone(&chrono::Utc);
        let dtstart = utc.format("%Y%m%dT%H%M%SZ");
        let dtend = (utc + chrono::Duration::hours(1)).format("%Y%m%dT%H%M%SZ");
        let summary = ics_escape(&format!(
            "{} S{:02}E{:02}{}",
            it.show_name,
            it.season,
            it.number,
            it.title.as_deref().map(|t| format!(" · {t}")).unwrap_or_default()
        ));
        out.push_str(&format!(
            "BEGIN:VEVENT\r\nUID:ep-{}-{}-{}@trawler\r\nDTSTAMP:{dtstart}\r\nDTSTART:{dtstart}\r\nDTEND:{dtend}\r\nSUMMARY:{summary}\r\nEND:VEVENT\r\n",
            it.show_id, it.season, it.number
        ));
    }
    out.push_str("END:VCALENDAR\r\n");
    out
}

/// Write upcoming episodes (next 90 days) to an .ics in Downloads and reveal it.
#[tauri::command]
pub async fn export_ical(state: State<'_, AppState>) -> Result<String> {
    let now = chrono::Utc::now();
    let start = now.format("%Y-%m-%d").to_string();
    let end = (now + chrono::Duration::days(90)).format("%Y-%m-%d").to_string();
    let items = calendar_range(state, start, end).await?;
    if items.is_empty() {
        return Err(AppError::Other("no upcoming episodes to export".into()));
    }
    let ics = build_ics(&items);
    let dir = dirs::download_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let path = dir.join("trawler-calendar.ics");
    std::fs::write(&path, ics)?;
    Ok(path.display().to_string())
}

// ---------------- discovery ----------------

const DISCOVER_CACHE_SECS: i64 = 6 * 3600;

async fn fetch_schedule_day(http: &reqwest::Client, kind: &str, date: &str) -> Vec<Value> {
    let url = match kind {
        "web" => format!("https://api.tvmaze.com/schedule/web?date={date}"),
        _ => format!("https://api.tvmaze.com/schedule?country=US&date={date}"),
    };
    match http.get(&url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) if r.status().is_success() => r.json::<Vec<Value>>().await.unwrap_or_default(),
        _ => vec![],
    }
}

fn slim_entry(e: &Value) -> Option<Value> {
    // /schedule embeds show directly; /schedule/web embeds under _embedded.show
    let show = e.get("show").or_else(|| e.pointer("/_embedded/show"))?;
    let id = show.get("id")?.as_i64()?;
    Some(json!({
        "showId": id,
        "name": show.get("name")?.as_str()?,
        "poster": show.pointer("/image/medium").and_then(|v| v.as_str()),
        "network": show.pointer("/network/name").or_else(|| show.pointer("/webChannel/name")).and_then(|v| v.as_str()),
        "status": show.get("status").and_then(|v| v.as_str()).unwrap_or("Running"),
        "weight": show.get("weight").and_then(|v| v.as_i64()).unwrap_or(0),
        "airstamp": e.get("airstamp").and_then(|v| v.as_str()),
        "season": e.get("season").and_then(|v| v.as_i64()),
        "number": e.get("number").and_then(|v| v.as_i64()),
        "episodeTitle": e.get("name").and_then(|v| v.as_str()),
    }))
}

/// The cache stores impersonal rows; the followed flag is stamped fresh per call.
fn stamp_followed(mut payload: Value, followed: &[i64]) -> Value {
    for row in ["tonight", "premieres", "popular"] {
        if let Some(arr) = payload.get_mut(row).and_then(|v| v.as_array_mut()) {
            for e in arr {
                let is = e
                    .get("showId")
                    .and_then(|v| v.as_i64())
                    .map(|id| followed.contains(&id))
                    .unwrap_or(false);
                e["followed"] = Value::Bool(is);
            }
        }
    }
    payload
}

fn dedupe_by_show(entries: Vec<Value>) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    entries
        .into_iter()
        .filter(|e| {
            e.get("showId")
                .and_then(|v| v.as_i64())
                .map(|id| seen.insert(id))
                .unwrap_or(false)
        })
        .collect()
}

fn sort_by_weight(mut entries: Vec<Value>) -> Vec<Value> {
    entries.sort_by_key(|e| -(e.get("weight").and_then(|v| v.as_i64()).unwrap_or(0)));
    entries
}

/// Three rows: airing tonight, this week's premieres, popular this week.
/// TVmaze schedule (broadcast + streaming), cached 6h.
#[tauri::command]
pub async fn discover(state: State<'_, AppState>, force: Option<bool>) -> Result<Value> {
    let followed: Vec<i64> = {
        let conn = state.db.lock().await;
        db::list_shows(&conn).unwrap_or_default().iter().map(|s| s.tvmaze_id).collect()
    };

    if !force.unwrap_or(false) {
        let cache = state.discover_cache.lock().await;
        if let Some((ts, cached)) = cache.as_ref() {
            if db::now() - ts < DISCOVER_CACHE_SECS {
                return Ok(stamp_followed(cached.clone(), &followed));
            }
        }
    }

    let today = chrono::Local::now();
    let dates: Vec<String> = (0..7)
        .map(|d| (today + chrono::Duration::days(d)).format("%Y-%m-%d").to_string())
        .collect();

    // 14 fetches (7 broadcast + 7 streaming), all parallel
    let mut futs = vec![];
    for date in &dates {
        futs.push(fetch_schedule_day(&state.http, "tv", date));
        futs.push(fetch_schedule_day(&state.http, "web", date));
    }
    let results = futures::future::join_all(futs).await;

    let mut tonight_raw = vec![];
    let mut week_raw = vec![];
    for (i, day) in results.into_iter().enumerate() {
        let is_today = i < 2;
        for e in &day {
            if let Some(slim) = slim_entry(e) {
                if is_today {
                    tonight_raw.push(slim.clone());
                }
                week_raw.push(slim);
            }
        }
    }

    let tonight = sort_by_weight(dedupe_by_show(tonight_raw)).into_iter().take(24).collect::<Vec<_>>();
    let premieres = sort_by_weight(dedupe_by_show(
        week_raw
            .iter()
            .filter(|e| e.get("number").and_then(|v| v.as_i64()) == Some(1))
            .cloned()
            .collect(),
    ))
    .into_iter()
    .take(24)
    .collect::<Vec<_>>();
    let popular = sort_by_weight(dedupe_by_show(week_raw)).into_iter().take(24).collect::<Vec<_>>();

    let out = json!({ "tonight": tonight, "premieres": premieres, "popular": popular });
    {
        let mut cache = state.discover_cache.lock().await;
        *cache = Some((db::now(), out.clone()));
    }
    Ok(stamp_followed(out, &followed))
}

// ---------------- show preview (discover detail card) ----------------

/// Decode numeric entities (&#8217; / &#x27;) in-place.
fn decode_numeric_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("&#") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let (digits, hex) = if let Some(t) = tail.strip_prefix(['x', 'X']) { (t, true) } else { (tail, false) };
        let end = digits.find(';');
        match end {
            Some(e) if e > 0 && e <= 6 => {
                let parsed = if hex {
                    u32::from_str_radix(&digits[..e], 16)
                } else {
                    digits[..e].parse::<u32>()
                };
                match parsed.ok().and_then(char::from_u32) {
                    Some(c) => {
                        out.push(c);
                        rest = &digits[e + 1..];
                    }
                    None => {
                        out.push_str("&#");
                        rest = tail;
                    }
                }
            }
            _ => {
                out.push_str("&#");
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Strip HTML tags and decode the entities TVmaze summaries actually use.
pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            // a space per closed tag keeps adjacent <p> blocks from gluing;
            // the whitespace collapse below tidies the doubles
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let named = decode_numeric_entities(&out)
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&rsquo;", "\u{2019}")
        .replace("&lsquo;", "\u{2018}")
        .replace("&ldquo;", "\u{201C}")
        .replace("&rdquo;", "\u{201D}")
        .replace("&hellip;", "\u{2026}")
        .replace("&mdash;", "\u{2014}")
        .replace("&ndash;", "\u{2013}")
        .replace("&eacute;", "\u{00E9}")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        // &amp; strictly LAST so double-escapes don't double-decode
        .replace("&amp;", "&");
    named.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Everything the discover detail card needs, in one TVmaze call.
#[tauri::command]
pub async fn show_preview(state: State<'_, AppState>, tvmaze_id: i64) -> Result<Value> {
    let url = format!("https://api.tvmaze.com/shows/{tvmaze_id}?embed[]=episodes&embed[]=cast");
    let resp = state
        .http
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(AppError::Other("TVmaze doesn't know this show anymore".into()));
    }
    let v: Value = resp
        .error_for_status()
        .map_err(|e| AppError::Other(format!("TVmaze error: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Other(format!("TVmaze lookup failed: {e}")))?;

    let episodes = v.pointer("/_embedded/episodes").and_then(|e| e.as_array());
    let episode_count = episodes.map(|e| e.len()).unwrap_or(0);
    // what the backfill checkbox actually grabs: only episodes that have aired
    let now_utc = chrono::Utc::now();
    let aired_episode_count = episodes
        .map(|eps| {
            eps.iter()
                .filter(|e| {
                    e.get("airstamp")
                        .and_then(|s| s.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|t| t <= now_utc)
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0);
    let season_count = episodes
        .map(|eps| {
            let mut seasons: Vec<i64> =
                eps.iter().filter_map(|e| e.get("season").and_then(|s| s.as_i64())).collect();
            seasons.sort_unstable();
            seasons.dedup();
            seasons.len()
        })
        .unwrap_or(0);

    let cast: Vec<Value> = v
        .pointer("/_embedded/cast")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .take(8)
                .filter_map(|m| {
                    Some(json!({
                        "person": m.pointer("/person/name")?.as_str()?,
                        "character": m.pointer("/character/name").and_then(|v| v.as_str()),
                        "image": m.pointer("/person/image/medium").and_then(|v| v.as_str()),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let followed = {
        let conn = state.db.lock().await;
        db::list_shows(&conn).unwrap_or_default().iter().any(|s| s.tvmaze_id == tvmaze_id)
    };

    let schedule_days = v
        .pointer("/schedule/days")
        .and_then(|d| d.as_array())
        .map(|d| d.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
        .filter(|s| !s.is_empty());

    Ok(json!({
        "tvmazeId": tvmaze_id,
        "name": v.get("name").and_then(|x| x.as_str()).unwrap_or("?"),
        "summary": v.get("summary").and_then(|x| x.as_str()).map(strip_html),
        "genres": v.get("genres").cloned().filter(|g| g.is_array()).unwrap_or(json!([])),
        "status": v.get("status").and_then(|x| x.as_str()).unwrap_or("Running"),
        "premiered": v.get("premiered").and_then(|x| x.as_str()),
        "ended": v.get("ended").and_then(|x| x.as_str()),
        "network": v.pointer("/network/name").or_else(|| v.pointer("/webChannel/name")).and_then(|x| x.as_str()),
        "runtime": v.get("averageRuntime").or_else(|| v.get("runtime")).and_then(|x| x.as_i64()),
        "rating": v.pointer("/rating/average").and_then(|x| x.as_f64()),
        "poster": v.pointer("/image/original").or_else(|| v.pointer("/image/medium")).and_then(|x| x.as_str()),
        "imdb": v.pointer("/externals/imdb").and_then(|x| x.as_str()),
        "scheduleDays": schedule_days,
        "scheduleTime": v.pointer("/schedule/time").and_then(|x| x.as_str()).filter(|s| !s.is_empty()),
        "episodeCount": episode_count,
        "airedEpisodeCount": aired_episode_count,
        "seasonCount": season_count,
        "cast": cast,
        "followed": followed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_cleans_tvmaze_summaries() {
        assert_eq!(
            strip_html("<p>A <b>dark</b> secret &amp; a &quot;plan&quot;.</p>\n<p>Second para.</p>"),
            "A dark secret & a \"plan\". Second para."
        );
        assert_eq!(strip_html(""), "");
        // unclosed tag doesn't eat the world
        assert_eq!(strip_html("safe <broken text"), "safe");
    }

    #[test]
    fn strip_html_separates_paragraphs_and_decodes_entities() {
        // adjacent tags must not glue words together
        assert_eq!(strip_html("<p>One.</p><p>Two.</p>"), "One. Two.");
        // &amp; decodes last: a double-escaped sequence decodes exactly once
        assert_eq!(strip_html("&amp;lt;"), "&lt;");
        // typographic + numeric entities TVmaze actually emits
        assert_eq!(strip_html("It&rsquo;s here&hellip;"), "It’s here…");
        assert_eq!(strip_html("Bob&#8217;s &#x27;show&#x27;"), "Bob’s 'show'");
        // malformed numerics pass through untouched
        assert_eq!(strip_html("&#zz; &#123456789;"), "&#zz; &#123456789;");
    }

    #[test]
    fn ics_is_valid_shape() {
        let items = vec![CalendarItem {
            show_id: 1,
            show_name: "Test; Show, One".into(),
            poster_url: None,
            season: 2,
            number: 5,
            title: Some("The, Episode".into()),
            airstamp: "2026-09-01T20:00:00+00:00".into(),
            local_date: "2026-09-01".into(),
            state: "upcoming".into(),
        }];
        let ics = build_ics(&items);
        assert!(ics.starts_with("BEGIN:VCALENDAR"));
        assert!(ics.contains("BEGIN:VEVENT"));
        assert!(ics.contains("DTSTART:20260901T200000Z"));
        assert!(ics.contains("SUMMARY:Test\\; Show\\, One S02E05 · The\\, Episode"));
        assert!(ics.trim_end().ends_with("END:VCALENDAR"));
        // bad airstamps are skipped, not fatal
        let mut bad = items;
        bad[0].airstamp = "garbage".into();
        assert!(!build_ics(&bad).contains("BEGIN:VEVENT"));
    }
}
