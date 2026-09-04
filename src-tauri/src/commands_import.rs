//! Watchlist import: paste show names, an IMDb list URL, or an IMDb CSV
//! export; each line is resolved against TVmaze with a confidence verdict,
//! ambiguous lines can be settled by one bounded LLM call (Rust-validated),
//! and the user confirms with a cost estimate before anything is followed.

use std::sync::Arc;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::State;

use crate::commands::normalize;
use crate::error::{AppError, Result};
use crate::AppState;

const MAX_LINES: usize = 50;
/// concurrency bound only — actual request pacing lives in tvmaze::rate_gate
const LOOKUP_CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportMatch {
    pub id: i64,
    pub name: String,
    pub premiered: Option<String>,
    pub network: Option<String>,
    pub status: String,
    pub poster: Option<String>,
    /// this candidate is already in the library
    pub followed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRow {
    /// stable identity for the frontend — input strings can collide
    pub idx: usize,
    /// the cleaned title as parsed from the user's input
    pub input: String,
    pub matches: Vec<ImportMatch>,
    /// preselected TVmaze id (top match) — None when nothing matched
    pub chosen: Option<i64>,
    /// "exact" | "good" | "ambiguous" | "none"
    pub confidence: String,
    /// the TVmaze lookup itself failed (rate limit, network) — distinct
    /// from "searched fine, found nothing"
    pub lookup_failed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportLookup {
    pub rows: Vec<ImportRow>,
    /// how many titles the input actually contained (rows is capped)
    pub total_found: usize,
}

/// One parsed input line: cleaned title plus an optional year hint.
#[derive(Debug, Clone, PartialEq)]
struct ParsedLine {
    title: String,
    year: Option<i32>,
}

fn parse_line(raw: &str) -> Option<ParsedLine> {
    let mut s = raw.trim();
    // strip list numbering and bullets: "12. ", "- ", "* "
    if let Some(rest) = s.split_once(". ").filter(|(n, _)| n.chars().all(|c| c.is_ascii_digit())) {
        s = rest.1.trim();
    }
    s = s.trim_start_matches(['-', '*', '•']).trim();
    if s.is_empty() {
        return None;
    }
    // trailing "(2017)" is a premiere-year hint, not part of the name
    let mut title = s.to_string();
    let mut year = None;
    if let Some(open) = s.rfind('(') {
        let inner: String = s[open + 1..].chars().take_while(|c| *c != ')').collect();
        if inner.len() == 4 && inner.chars().all(|c| c.is_ascii_digit()) {
            year = inner.parse::<i32>().ok().filter(|y| (1930..=2035).contains(y));
            if year.is_some() {
                title = s[..open].trim().to_string();
            }
        }
    }
    if title.is_empty() {
        return None;
    }
    Some(ParsedLine { title: title.chars().take(120).collect(), year })
}

/// Minimal CSV field splitter that honors double quotes (IMDb exports).
fn split_csv(line: &str) -> Vec<String> {
    let mut fields = vec![];
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => fields.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    fields.push(cur);
    fields
}

/// Join physical lines whose quotes are unbalanced — a quoted CSV field
/// (IMDb's Description column) may contain literal newlines.
fn logical_csv_lines(input: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    for line in input.lines() {
        if cur.is_empty() {
            cur = line.to_string();
        } else {
            cur.push('\n');
            cur.push_str(line);
        }
        if cur.chars().filter(|c| *c == '"').count() % 2 == 0 {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// IMDb CSV export → (TV titles, total data rows seen).
fn parse_imdb_csv(input: &str) -> Option<(Vec<ParsedLine>, usize)> {
    let mut lines = logical_csv_lines(input).into_iter();
    let header = split_csv(&lines.next()?);
    let col = |name: &str| header.iter().position(|h| h.trim().eq_ignore_ascii_case(name));
    let title_col = col("Title")?;
    let type_col = col("Title Type");
    let year_col = col("Year");
    let mut out = vec![];
    let mut total = 0usize;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let fields = split_csv(&line);
        let Some(title) = fields.get(title_col).map(|t| t.trim()).filter(|t| !t.is_empty()) else {
            continue;
        };
        total += 1;
        if let Some(tc) = type_col {
            let kind = fields.get(tc).map(|t| t.trim().to_lowercase()).unwrap_or_default();
            // movies and shorts can't be followed; podcasts aren't TV
            if !kind.is_empty() && (!kind.contains("series") || kind.contains("podcast")) {
                continue;
            }
        }
        let year = year_col
            .and_then(|yc| fields.get(yc))
            .and_then(|y| y.trim().parse::<i32>().ok());
        out.push(ParsedLine { title: title.chars().take(120).collect(), year });
    }
    Some((out, total))
}

/// First double quote not preceded by an odd run of backslashes.
fn find_unescaped_quote(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'"' {
            let mut backslashes = 0;
            let mut j = i;
            while j > 0 && b[j - 1] == b'\\' {
                backslashes += 1;
                j -= 1;
            }
            if backslashes % 2 == 0 {
                return Some(i);
            }
        }
    }
    None
}

fn is_imdb_list_url(s: &str) -> bool {
    let Ok(u) = url::Url::parse(s) else { return false };
    let host_ok = matches!(u.host_str(), Some("imdb.com" | "www.imdb.com" | "m.imdb.com"));
    let path = u.path();
    host_ok && (path.starts_with("/list/") || (path.starts_with("/user/") && path.contains("/watchlist")))
}

/// Best-effort scrape of a public IMDb list/watchlist page. IMDb renders from
/// embedded JSON; both the JSON-LD block and the Next.js payload carry titles.
async fn fetch_imdb_titles(http: &reqwest::Client, url: &str) -> Result<Vec<ParsedLine>> {
    let resp = http
        .get(url)
        .header("Accept", "text/html")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36",
        )
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(AppError::Other(format!(
            "IMDb returned {} — if the list is private, use its Export button and paste the CSV here instead",
            resp.status()
        )));
    }
    let html = resp.text().await?;

    let mut titles: Vec<String> = vec![];
    // 1) JSON-LD ItemList
    for blob in html.split("<script type=\"application/ld+json\">").skip(1) {
        let Some(json_str) = blob.split("</script>").next() else { continue };
        let Ok(v) = serde_json::from_str::<Value>(json_str) else { continue };
        if let Some(items) = v.get("itemListElement").and_then(|i| i.as_array()) {
            for item in items {
                if let Some(name) = item
                    .pointer("/item/name")
                    .or_else(|| item.get("name"))
                    .and_then(|n| n.as_str())
                {
                    titles.push(html_unescape(name));
                }
            }
        }
    }
    // 2) Next.js data payload ("titleText":{"text":"..."})
    if titles.is_empty() {
        for chunk in html.split("\"titleText\":{\"text\":\"").skip(1) {
            if let Some(end) = find_unescaped_quote(chunk) {
                let raw = &chunk[..end];
                // the payload escapes with \uXXXX / \" — decode via serde
                if let Ok(Value::String(s)) = serde_json::from_str::<Value>(&format!("\"{raw}\"")) {
                    titles.push(s);
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    let titles: Vec<ParsedLine> = titles
        .into_iter()
        .filter_map(|t| parse_line(&t))
        // dedupe AFTER the year hint splits off, so same-name remakes survive
        .filter(|p| seen.insert((normalize(&p.title), p.year)))
        .collect();
    if titles.is_empty() {
        return Err(AppError::Other(
            "couldn't read titles from that IMDb page — use the list's Export button and paste the CSV here instead".into(),
        ));
    }
    Ok(titles)
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&#x27;", "'").replace("&quot;", "\"")
}

/// Parse whatever the user pasted into lookup candidates.
async fn parse_input(http: &reqwest::Client, input: &str) -> Result<Vec<ParsedLine>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(AppError::Other("paste some show names first".into()));
    }
    // one URL on its own line → IMDb list scrape (lists only, not title pages)
    if trimmed.lines().count() == 1 && trimmed.contains("imdb.com") {
        if is_imdb_list_url(trimmed) {
            return fetch_imdb_titles(http, trimmed).await;
        }
        return Err(AppError::Other(
            "that IMDb link isn't a list — open your list or watchlist and copy THAT page's URL, or use its Export button and paste the CSV".into(),
        ));
    }
    // IMDb CSV export (has a Title header column)
    if let Some((rows, total)) = parse_imdb_csv(trimmed) {
        if !rows.is_empty() {
            return Ok(rows);
        }
        if total > 0 {
            return Err(AppError::Other(
                "that export has no TV series in it — only movies or podcasts, which Trawler can't follow".into(),
            ));
        }
        return Err(AppError::Other("that CSV has a Title column but no usable rows".into()));
    }
    // CSV-shaped input without a Title header would parse into garbage lines
    {
        let lines: Vec<&str> = trimmed.lines().take(5).collect();
        if lines.len() >= 2 {
            let counts: Vec<usize> = lines.iter().map(|l| split_csv(l).len()).collect();
            if counts[0] >= 3 && counts.iter().all(|c| *c == counts[0]) {
                return Err(AppError::Other(
                    "that looks like a CSV without a \"Title\" column — export from IMDb, or paste plain show names".into(),
                ));
            }
        }
    }
    // plain lines
    let mut seen = std::collections::HashSet::new();
    Ok(trimmed
        .lines()
        .filter_map(parse_line)
        .filter(|p| seen.insert((normalize(&p.title), p.year)))
        .collect())
}

fn year_of(premiered: Option<&str>) -> Option<i32> {
    premiered.and_then(|p| p.get(..4)).and_then(|y| y.parse().ok())
}

/// Decide confidence and the preselected match for one line.
fn judge(line: &ParsedLine, matches: &[ImportMatch]) -> (Option<i64>, String) {
    if matches.is_empty() {
        return (None, "none".into());
    }
    let want = normalize(&line.title);
    let exact: Vec<&ImportMatch> = matches.iter().filter(|m| normalize(&m.name) == want).collect();
    if let Some(y) = line.year {
        // a year hint settles same-name remakes
        if let Some(m) = exact.iter().find(|m| year_of(m.premiered.as_deref()) == Some(y)) {
            return (Some(m.id), "exact".into());
        }
        // the year disagrees with every candidate it could confirm — that's
        // evidence of confusion, not confidence. Preselect the least-bad
        // option but demand review either way.
        if let Some(first_exact) = exact.first() {
            return (Some(first_exact.id), "ambiguous".into());
        }
        if let Some(m) = matches.iter().find(|m| year_of(m.premiered.as_deref()) == Some(y)) {
            return (Some(m.id), "ambiguous".into());
        }
    }
    match exact.as_slice() {
        [only] => return (Some(only.id), "exact".into()),
        [first, ..] => return (Some(first.id), "ambiguous".into()), // several same-name shows
        [] => {}
    }
    if matches.len() == 1 {
        return (Some(matches[0].id), "good".into());
    }
    (Some(matches[0].id), "ambiguous".into())
}

/// Resolve pasted input into per-line TVmaze candidates.
#[tauri::command]
pub async fn import_lookup(state: State<'_, AppState>, input: String) -> Result<ImportLookup> {
    let lines = parse_input(&state.http, &input).await?;
    if lines.is_empty() {
        return Err(AppError::Other("no show names found in that input".into()));
    }
    let total_found = lines.len();
    let lines: Vec<ParsedLine> = lines.into_iter().take(MAX_LINES).collect();

    let followed_ids: Arc<std::collections::HashSet<i64>> = Arc::new({
        let conn = state.db.lock().await;
        crate::db::list_shows(&conn)
            .unwrap_or_default()
            .iter()
            .map(|s| s.tvmaze_id)
            .collect()
    });

    let http = state.http.clone();
    let rows: Vec<ImportRow> =
        futures::stream::iter(lines.into_iter().enumerate().map(|(idx, line)| {
            let http = http.clone();
            let followed_ids = followed_ids.clone();
            async move {
                let result = crate::tvmaze::search_shows(&http, &line.title).await;
                let lookup_failed = result.is_err();
                let matches: Vec<ImportMatch> = result
                    .unwrap_or_default()
                    .into_iter()
                    .take(4)
                    .map(|s| ImportMatch {
                        followed: followed_ids.contains(&s.id),
                        id: s.id,
                        poster: s.poster(),
                        network: s.network_name(),
                        name: s.name,
                        premiered: s.premiered,
                        status: s.status,
                    })
                    .collect();
                let (chosen, confidence) = judge(&line, &matches);
                ImportRow { idx, input: line.title, matches, chosen, confidence, lookup_failed }
            }
        }))
        .buffered(LOOKUP_CONCURRENCY)
        .collect()
        .await;

    Ok(ImportLookup { rows, total_found })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportEstimate {
    pub shows: usize,
    pub aired_episodes: i64,
    pub est_gb: f64,
    /// shows whose episode list couldn't be fetched — the estimate is partial
    pub failed: usize,
}

/// How much would following these shows (with backfill) actually download?
#[tauri::command]
pub async fn import_estimate(state: State<'_, AppState>, ids: Vec<i64>) -> Result<ImportEstimate> {
    let ids: Vec<i64> = ids.into_iter().take(MAX_LINES).collect();
    let now = chrono::Utc::now();
    let http = state.http.clone();
    let counts: Vec<Option<i64>> = futures::stream::iter(ids.clone().into_iter().map(|id| {
        let http = http.clone();
        async move {
            crate::tvmaze::show_with_episodes(&http, id).await.ok().map(|s| {
                s.embedded
                    .map(|e| {
                        e.episodes
                            .iter()
                            .filter(|ep| {
                                ep.airstamp
                                    .as_deref()
                                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                                    .map(|t| t <= now)
                                    .unwrap_or(false)
                            })
                            .count() as i64
                    })
                    .unwrap_or(0)
            })
        }
    }))
    .buffered(LOOKUP_CONCURRENCY)
    .collect()
    .await;

    let aired: i64 = counts.iter().flatten().sum();
    let failed = counts.iter().filter(|c| c.is_none()).count();
    let per_ep_gb = {
        let cfg = state.config.read().await;
        let q = &cfg.default_quality;
        // expected size by resolution; the profile cap is a ceiling, not a size
        let base: f64 = if q.resolutions.is_empty() || q.resolutions.iter().any(|r| r == "2160p") {
            5.0
        } else if q.resolutions.iter().any(|r| r == "1080p") {
            2.5
        } else {
            1.2
        };
        if q.max_size_gb > 0.0 { base.min(q.max_size_gb) } else { base }
    };
    Ok(ImportEstimate {
        shows: ids.len(),
        aired_episodes: aired,
        est_gb: aired as f64 * per_ep_gb,
        failed,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AmbiguousRow {
    pub input: String,
    pub candidates: Vec<AmbiguousCandidate>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AmbiguousCandidate {
    pub id: i64,
    pub name: String,
    pub year: Option<String>,
    pub network: Option<String>,
    pub status: String,
}

/// One bounded LLM call to settle ambiguous rows. The model only ever picks
/// among the candidate ids we hand it — anything else is discarded in Rust.
#[tauri::command]
pub async fn import_disambiguate(
    state: State<'_, AppState>,
    rows: Vec<AmbiguousRow>,
) -> Result<Vec<Option<i64>>> {
    let cfg = state.config.read().await.clone();
    if !cfg.agent_enabled {
        return Err(AppError::Other("the agent is disabled in Settings".into()));
    }
    let rows: Vec<AmbiguousRow> = rows.into_iter().take(MAX_LINES).collect();
    if rows.is_empty() {
        return Ok(vec![]);
    }

    let mut listing = String::new();
    for (i, row) in rows.iter().enumerate() {
        listing.push_str(&format!("{}. \"{}\"\n", i, row.input.chars().take(80).collect::<String>()));
        for c in row.candidates.iter().take(4) {
            listing.push_str(&format!(
                "   - id {}: {} ({}, {}, {})\n",
                c.id,
                c.name.chars().take(60).collect::<String>(),
                c.year
                    .as_deref()
                    .map(|y| y.chars().take(4).collect::<String>())
                    .unwrap_or_else(|| "?".into()),
                c.network.as_deref().unwrap_or("?"),
                c.status
            ));
        }
    }
    let system = "A user is importing a TV watchlist. For each numbered input line, pick the candidate the user most likely means (prefer the famous/flagship show over documentaries, aftershows and same-name obscurities; a year in the input is decisive). Reply with ONLY a JSON object mapping the line number to the chosen candidate id, e.g. {\"0\": 123, \"1\": 456}. If genuinely unguessable, omit that line.";

    let client = crate::llm::LlmClient::new(&cfg.agent_base_url, &cfg.agent_model);
    let reply = client
        .chat(
            &[
                crate::llm::ChatMsg::system(system),
                crate::llm::ChatMsg::user(listing),
            ],
            None,
        )
        .await?;
    let text = reply.content.unwrap_or_default();
    // shared with brief compilation: fences, prose and <think> blocks
    // tolerated, and a miss parses as an empty map below
    let json_str = crate::llm::extract_json_object(&text);
    // decode per entry: one malformed value must not void the whole batch
    let raw: std::collections::HashMap<String, Value> =
        serde_json::from_str(json_str).unwrap_or_default();
    let picks: std::collections::HashMap<String, i64> = raw
        .into_iter()
        .filter_map(|(k, v)| {
            let id = v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))?;
            Some((k, id))
        })
        .collect();

    // Rust rail: a pick is only honored when it names one of that row's candidates.
    Ok(rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            picks
                .get(&i.to_string())
                .copied()
                .filter(|id| row.candidates.iter().any(|c| c.id == *id))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_lines() {
        assert_eq!(
            parse_line("  3. The Expanse "),
            Some(ParsedLine { title: "The Expanse".into(), year: None })
        );
        assert_eq!(
            parse_line("- Dark (2017)"),
            Some(ParsedLine { title: "Dark".into(), year: Some(2017) })
        );
        assert_eq!(parse_line("   "), None);
        // parenthetical that isn't a year stays in the title
        assert_eq!(
            parse_line("Shogun (miniseries)"),
            Some(ParsedLine { title: "Shogun (miniseries)".into(), year: None })
        );
    }

    #[test]
    fn parses_imdb_csv() {
        let csv = "Position,Const,Title,Title Type,Year\n1,tt1,\"Dark\",TV Series,2017\n2,tt2,\"Dune, Part Two\",Movie,2024\n3,tt3,Severance,TV Mini Series,2022\n";
        let (rows, total) = parse_imdb_csv(csv).unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows.len(), 2); // the movie is filtered out
        assert_eq!(rows[0], ParsedLine { title: "Dark".into(), year: Some(2017) });
        assert_eq!(rows[1].title, "Severance");
        // case-insensitive headers; podcasts excluded
        let csv2 = "position,const,title,title type,year\n1,tt1,SomePod,Podcast Series,2020\n2,tt2,RealShow,TV Series,2021\n";
        let (rows2, total2) = parse_imdb_csv(csv2).unwrap();
        assert_eq!((rows2.len(), total2), (1, 2));
        assert_eq!(rows2[0].title, "RealShow");
        // quoted field with an embedded newline doesn't break the record
        let csv3 = "Title,Title Type,Description\n\"Dark\",TV Series,\"line one\nline two\"\n";
        let (rows3, _) = parse_imdb_csv(csv3).unwrap();
        assert_eq!(rows3.len(), 1);
    }

    #[test]
    fn csv_quoting() {
        assert_eq!(
            split_csv("a,\"b, c\",\"say \"\"hi\"\"\",d"),
            vec!["a", "b, c", "say \"hi\"", "d"]
        );
    }

    #[test]
    fn imdb_url_gate() {
        assert!(is_imdb_list_url("https://www.imdb.com/list/ls091520106/"));
        assert!(is_imdb_list_url("https://imdb.com/user/ur1234567/watchlist"));
        assert!(!is_imdb_list_url("https://www.imdb.com/title/tt4574334/"));
        assert!(!is_imdb_list_url("https://evil.example/imdb.com/list/x"));
        assert!(!is_imdb_list_url("not a url imdb.com/list/"));
    }

    #[test]
    fn unescaped_quote_scan() {
        // \"Weird Al\" — the first REAL quote terminates after the escaped ones
        let chunk = r#"\"Weird Al\" Yankovic","x":1"#;
        let end = find_unescaped_quote(chunk).unwrap();
        assert_eq!(&chunk[..end], r#"\"Weird Al\" Yankovic"#);
    }

    fn m(id: i64, name: &str, year: i32) -> ImportMatch {
        ImportMatch {
            id,
            name: name.into(),
            premiered: Some(format!("{year}-01-01")),
            network: None,
            status: "Ended".into(),
            poster: None,
            followed: false,
        }
    }

    #[test]
    fn judge_confidence() {
        let line = |t: &str, y: Option<i32>| ParsedLine { title: t.into(), year: y };
        // single exact name match
        let (c, conf) = judge(&line("Dark", None), &[m(1, "Dark", 2017), m(2, "Dark Matter", 2024)]);
        assert_eq!((c, conf.as_str()), (Some(1), "exact"));
        // year settles same-name remakes
        let (c, conf) =
            judge(&line("Battlestar Galactica", Some(2004)), &[m(1, "Battlestar Galactica", 1978), m(2, "Battlestar Galactica", 2004)]);
        assert_eq!((c, conf.as_str()), (Some(2), "exact"));
        // several same-name shows, no year → ambiguous
        let (c, conf) =
            judge(&line("Battlestar Galactica", None), &[m(1, "Battlestar Galactica", 1978), m(2, "Battlestar Galactica", 2004)]);
        assert_eq!((c, conf.as_str()), (Some(1), "ambiguous"));
        // exact name but the year hint disagrees → preselect the exact name,
        // demand review ("Dark (2015)" must not silently become Dark Matter,
        // nor silently be trusted as Dark)
        let (c, conf) = judge(&line("Dark", Some(2015)), &[m(1, "Dark", 2017), m(2, "Dark Matter", 2015)]);
        assert_eq!((c, conf.as_str()), (Some(1), "ambiguous"));
        // no exact name at all: the year picks a candidate but still reviews
        let (c, conf) = judge(&line("Drk", Some(2015)), &[m(1, "Dark", 2017), m(2, "Dark Matter", 2015)]);
        assert_eq!((c, conf.as_str()), (Some(2), "ambiguous"));
        // nothing found
        let (c, conf) = judge(&line("zzz", None), &[]);
        assert_eq!((c, conf.as_str()), (None, "none"));
        // fuzzy-only single result
        let (c, conf) = judge(&line("expanse", None), &[m(9, "The Expanse", 2015)]);
        assert_eq!((c, conf.as_str()), (Some(9), "good"));
    }
}
