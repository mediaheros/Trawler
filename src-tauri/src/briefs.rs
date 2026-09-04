//! Standing briefs: natural language compiled ONCE into a structured HuntPlan
//! the user confirms; every grab is then re-validated against the plan in Rust,
//! so model misinterpretation can never widen scope after confirmation.

use serde::{Deserialize, Serialize};
use crate::error::{AppError, Result};
use crate::llm::{ChatMsg, LlmClient};
use crate::parse;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct HuntPlan {
    /// seed search queries derived from the brief (bounded; model picks among these)
    pub queries: Vec<String>,
    /// normalized tokens that MUST all appear in a release name
    pub include: Vec<String>,
    /// normalized tokens that must NOT appear
    pub exclude: Vec<String>,
    /// allowed resolutions; empty = any
    pub resolutions: Vec<String>,
    /// per-item size cap in GB; 0 = none
    pub max_size_gb: f64,
    pub min_seeders: i64,
    /// free-text guidance for the judging model (user-confirmed)
    pub notes: String,
}

impl Default for HuntPlan {
    fn default() -> Self {
        Self {
            queries: vec![],
            include: vec![],
            exclude: vec![],
            resolutions: vec![],
            max_size_gb: 0.0,
            min_seeders: 1,
            notes: String::new(),
        }
    }
}

fn norm_token(s: &str) -> String {
    // separators become SPACES, never nothing: scene names are dot-separated,
    // so deleting dots glued "UFC.319.Main.Card" into one unmatchable run
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whole-word phrase match over normalized text: "it" must not fire on
/// "split", and "early" must not fire on "pearly". Multi-word terms match as
/// consecutive words ("main card" ↔ "UFC.319.Main.Card").
fn contains_phrase(haystack: &str, phrase: &str) -> bool {
    let haystack: Vec<&str> = haystack.split_whitespace().collect();
    let phrase: Vec<&str> = phrase.split_whitespace().collect();
    !phrase.is_empty()
        && phrase.len() <= haystack.len()
        && haystack.windows(phrase.len()).any(|window| window == phrase)
}

/// Does one plan term describe this release? Whole words first; then the
/// scene conventions a plain word match would miss: a season marker extends
/// to its episodes ("s02" ↔ "s02e05"), a term of four or more characters
/// matches the word it starts ("prelim" ↔ "prelims"), and quality families
/// go through the parser so "cam" catches HDCAM/CAMRip and "hdr" sees HDR10.
fn term_matches(norm_title: &str, parsed: &parse::ParsedRelease, term: &str) -> bool {
    if contains_phrase(norm_title, term) {
        return true;
    }
    let [word] = term.split_whitespace().collect::<Vec<_>>()[..] else {
        return false;
    };
    let mut title_words = norm_title.split_whitespace();
    if title_words.any(|w| crate::commands::word_matches(w, word)) {
        return true;
    }
    if word.chars().count() >= 4 && norm_title.split_whitespace().any(|w| w.starts_with(word)) {
        return true;
    }
    let family = |value: &Option<String>| {
        value
            .as_deref()
            .map(|v| v.to_lowercase().replace(['-', ':', '+'], " ").trim().to_string())
    };
    match word {
        "hdr" => parsed.hdr.is_some(),
        "dv" => parsed.hdr.as_deref() == Some("DV"),
        _ => [&parsed.source, &parsed.codec, &parsed.resolution, &parsed.hdr]
            .into_iter()
            .any(|field| family(field).as_deref() == Some(word)),
    }
}

impl HuntPlan {
    /// Clamp everything a model could inflate. Called after compile AND after edits.
    pub fn sanitize(mut self) -> Self {
        self.queries.truncate(6);
        self.queries.retain(|q| !q.trim().is_empty());
        for q in &mut self.queries {
            *q = q.chars().take(80).collect();
        }
        self.include = self.include.iter().map(|t| norm_token(t)).filter(|t| !t.is_empty()).take(8).collect();
        self.exclude = self.exclude.iter().map(|t| norm_token(t)).filter(|t| !t.is_empty()).take(12).collect();
        self.resolutions.retain(|r| ["2160p", "1080p", "720p", "480p"].contains(&r.as_str()));
        if self.max_size_gb < 0.0 || !self.max_size_gb.is_finite() {
            self.max_size_gb = 0.0;
        }
        self.max_size_gb = self.max_size_gb.min(500.0);
        self.min_seeders = self.min_seeders.clamp(0, 1000);
        self.notes = self.notes.chars().take(500).collect();
        self
    }

    /// Rust-side verdict: does this release satisfy the compiled constraints?
    /// Returns Err(reason) on violation — reasons surface in verdict cards.
    pub fn allows(&self, title: &str, size: i64, seeders: Option<i32>) -> std::result::Result<(), String> {
        let norm_title = norm_token(title);
        let parsed = parse::parse(title);
        for tok in &self.include {
            if !term_matches(&norm_title, &parsed, tok) {
                return Err(format!("missing required term \"{tok}\""));
            }
        }
        for tok in &self.exclude {
            if term_matches(&norm_title, &parsed, tok) {
                return Err(format!("contains excluded term \"{tok}\""));
            }
        }
        if !self.resolutions.is_empty() {
            match parsed.resolution.as_deref() {
                Some(res) if self.resolutions.iter().any(|r| r == res) => {}
                Some(res) => return Err(format!("{res} not in allowed resolutions")),
                None => {} // unknown resolution passes; ranking handles it
            }
        }
        if self.max_size_gb > 0.0 && size > 0 && size as f64 > self.max_size_gb * 1e9 {
            return Err(format!(
                "{:.1} GB exceeds the {:.0} GB cap",
                size as f64 / 1e9,
                self.max_size_gb
            ));
        }
        if self.min_seeders > 0 {
            match seeders {
                Some(s) if (s as i64) < self.min_seeders => {
                    return Err(format!("only {s} seeders (minimum {})", self.min_seeders))
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn content_base(clean_title: &str) -> String {
    // Every alphanumeric script survives and diacritics fold to ASCII: the old
    // ASCII-only filter emptied non-Latin titles, so every such show's S01E01
    // shared one content key and only the first was ever grabbed. Punctuation
    // is dropped, not spaced, exactly as before - keys already stored in
    // users' ledgers ("its always sunny") must keep matching.
    let mut out = String::with_capacity(clean_title.len());
    for c in clean_title.chars() {
        let lower = c.to_lowercase().next().unwrap_or(c);
        if c.is_whitespace() {
            out.push(' ');
        } else if let Some(folded) = crate::commands::fold_diacritic(lower) {
            out.push_str(folded);
        } else if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Content identity for semantic dedupe: survives group/REPACK/indexer variants.
/// TV: show + episode/season. Everything else: normalized clean title (+ year).
pub fn content_key(title: &str) -> String {
    let p = parse::parse(title);
    let base = content_base(&p.clean_title);
    match (p.season, p.episode) {
        (Some(s), Some(e)) => format!("tv:{base}:s{s:02}e{e:02}"),
        (Some(s), None) => format!("tv:{base}:s{s:02}pack"),
        _ => match p.year {
            Some(y) => format!("item:{base}:{y}"),
            None => format!("item:{base}"),
        },
    }
}

/// The key a single-episode release of this content would get — used when the
/// source title is a season pack whose own key is the pack, not the episode.
pub fn content_key_for_episode(title: &str, season: i64, episode: i64) -> String {
    let p = parse::parse(title);
    let base = content_base(&p.clean_title);
    format!("tv:{base}:s{season:02}e{episode:02}")
}

const COMPILE_PROMPT: &str = r#"You compile a user's media-hunting brief into a strict JSON plan. Output ONLY a JSON object, no prose, with exactly these fields:
{
  "queries": [up to 5 short search strings a torrent indexer would match; use scene-style naming, no punctuation],
  "include": [words that MUST appear in every acceptable release name, lowercase],
  "exclude": [words that disqualify a release, lowercase],
  "resolutions": [any of "2160p","1080p","720p","480p"; empty array = any],
  "maxSizeGb": number (0 = no cap),
  "minSeeders": number,
  "notes": "one or two sentences of judgment guidance for the hunting agent"
}
Be conservative: include only terms clearly implied by the brief. If the user names a size limit, quality, or things to avoid, encode them. The include list is a hard filter — put ONLY terms that will appear in every valid release name (e.g. "ufc"), NOT descriptive words like "main card" that release names may omit; put soft preferences in notes."#;

/// One LLM call turning a natural-language brief into a HuntPlan (then sanitized).
pub async fn compile(client: &LlmClient, brief_prompt: &str) -> Result<HuntPlan> {
    let mut messages = vec![
        ChatMsg::system(COMPILE_PROMPT),
        ChatMsg::user(format!("Brief: {}", brief_prompt.chars().take(1000).collect::<String>())),
    ];
    let mut last_err = String::new();
    let mut last_reply = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            // a second identical request to a deterministic backend fails the
            // same way; show the model its own reply and what was wrong with
            // it. The assistant turn keeps roles alternating — backends with
            // strict chat templates reject user,user with HTTP 400.
            messages.push(ChatMsg::assistant_text(last_reply.clone()));
            messages.push(ChatMsg::user(format!(
                "That reply could not be used ({last_err}). Reply with ONLY the JSON object, no prose, no code fence."
            )));
        }
        let reply = client.chat(&messages, None).await?;
        let text = reply.content.unwrap_or_default();
        let json_str = crate::llm::extract_json_object(&text);
        last_reply = text.chars().take(4000).collect();
        match serde_json::from_str::<HuntPlan>(json_str) {
            Ok(plan) => {
                let plan = plan.sanitize();
                if plan.queries.is_empty() {
                    last_err = "compiled plan had no queries".into();
                    continue;
                }
                return Ok(plan);
            }
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        }
    }
    Err(AppError::Other(format!("could not compile the brief into a plan: {last_err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> HuntPlan {
        HuntPlan {
            queries: vec!["ufc 319".into()],
            include: vec!["ufc".into()],
            exclude: vec!["prelims".into(), "early".into()],
            resolutions: vec!["1080p".into()],
            max_size_gb: 6.0,
            min_seeders: 5,
            notes: String::new(),
        }
    }

    use crate::llm::extract_json_object;

    #[test]
    fn extract_json_object_never_panics_on_odd_brace_order() {
        // last '}' before first '{' — the slice that used to abort the app
        assert_eq!(extract_json_object("prose } then {\"a\":1"), "{\"a\":1");
        // think block with a brace, then a real object
        assert_eq!(
            extract_json_object("<think>plan {x}</think>\n```json\n{\"a\":1}\n```"),
            "{\"a\":1}"
        );
        // no braces at all falls through untouched
        assert_eq!(extract_json_object("no json here"), "no json here");
        // fences and leading prose are tolerated
        assert_eq!(extract_json_object("Sure:\n```json\n{\"q\":[]}\n```"), "{\"q\":[]}");
    }

    #[test]
    fn plan_gates_correctly() {
        let p = plan();
        assert!(p.allows("UFC.319.Main.Card.1080p.WEB.h264-VERUM", 4_000_000_000, Some(50)).is_ok());
        // dot-separated multi-word include now matches (regression: it didn't)
        {
            let mut mw = plan();
            mw.include = vec!["main card".into()];
            assert!(mw.allows("UFC.319.Main.Card.1080p.WEB", 4_000_000_000, Some(50)).is_ok());
            assert!(mw.allows("UFC.319.Prelims.1080p.WEB", 4_000_000_000, Some(50)).is_err());
        }
        // missing required term
        assert!(p.allows("Bellator.300.1080p.WEB", 4_000_000_000, Some(50)).is_err());
        // excluded term
        assert!(p.allows("UFC.319.Early.Prelims.1080p.WEB", 4_000_000_000, Some(50)).is_err());
        // wrong resolution
        assert!(p.allows("UFC.319.Main.Card.2160p.WEB", 4_000_000_000, Some(50)).is_err());
        // size cap — the 200GB-of-prelims case
        assert!(p.allows("UFC.319.Main.Card.1080p.WEB", 190_000_000_000, Some(50)).is_err());
        // seeder floor
        assert!(p.allows("UFC.319.Main.Card.1080p.WEB", 4_000_000_000, Some(2)).is_err());
        // unknown resolution passes the res filter
        assert!(p.allows("UFC.319.Main.Card.WEB.h264", 4_000_000_000, Some(50)).is_ok());
    }

    #[test]
    fn plan_terms_match_whole_words_not_substrings() {
        // a brief for "It" must never accept "Split" as the same title
        let mut short = plan();
        short.include = vec!["it".into()];
        short.exclude.clear();
        assert!(short.allows("It.2017.1080p.WEB", 1_000_000_000, Some(10)).is_ok());
        assert!(short.allows("Split.2017.1080p.WEB", 1_000_000_000, Some(10)).is_err());
        // an excluded word must not fire on a longer word that contains it
        let mut ex = plan();
        ex.include = vec!["ufc".into()];
        ex.exclude = vec!["early".into()];
        assert!(ex.allows("UFC.319.Pearly.Gates.1080p.WEB", 1_000_000_000, Some(10)).is_ok());
        assert!(ex.allows("UFC.319.Early.Prelims.1080p.WEB", 1_000_000_000, Some(10)).is_err());
        // a term of four or more characters still matches the word it starts
        // ("prelim" vs "prelims"), a season term matches its episode marker,
        // and scene quality families match through the parser (a "cam"
        // exclusion must catch HDCAM/CAMRip, "hdr" must see HDR10)
        let mut fam = plan();
        fam.include = vec!["ufc".into()];
        fam.exclude = vec!["prelim".into()];
        assert!(fam.allows("UFC.319.Prelims.1080p.WEB", 1_000_000_000, Some(10)).is_err());
        let mut season = plan();
        season.include = vec!["s02".into()];
        season.exclude.clear();
        assert!(season.allows("Show.S02E05.1080p.WEB", 1_000_000_000, Some(10)).is_ok());
        assert!(season.allows("Show.S03E05.1080p.WEB", 1_000_000_000, Some(10)).is_err());
        let mut cam = plan();
        cam.include.clear();
        cam.exclude = vec!["cam".into()];
        assert!(cam.allows("Movie.2024.HDCAM.x264", 1_000_000_000, Some(10)).is_err());
        assert!(cam.allows("Movie.2024.CAMRip.x264", 1_000_000_000, Some(10)).is_err());
        assert!(cam.allows("Camera.Obscura.2024.1080p.WEB", 1_000_000_000, Some(10)).is_ok());
        let mut hdr = plan();
        hdr.include = vec!["hdr".into()];
        hdr.exclude.clear();
        hdr.resolutions.clear();
        assert!(hdr.allows("Movie.2024.2160p.HDR10.WEB", 1_000_000_000, Some(10)).is_ok());
        assert!(hdr.allows("Movie.2024.2160p.SDR.WEB", 1_000_000_000, Some(10)).is_err());
    }

    #[test]
    fn sanitize_clamps_model_inflation() {
        let p = HuntPlan {
            queries: (0..20).map(|i| format!("q{i}")).collect(),
            include: vec!["<script>UFC</script>".into()],
            exclude: vec![],
            resolutions: vec!["1080p".into(), "999p".into()],
            max_size_gb: -3.0,
            min_seeders: 99999,
            notes: "x".repeat(9000),
        }
        .sanitize();
        assert_eq!(p.queries.len(), 6);
        assert_eq!(p.include, vec!["script ufc script"]); // tags → spaces, injection neutralized
        assert_eq!(p.resolutions, vec!["1080p"]);
        assert_eq!(p.max_size_gb, 0.0);
        assert_eq!(p.min_seeders, 1000);
        assert_eq!(p.notes.chars().count(), 500);
    }

    #[test]
    fn content_keys_dedupe_variants() {
        // same episode, different groups/quality → same key
        assert_eq!(
            content_key("Severance.S02E07.1080p.WEB-DL.x265-NTb"),
            content_key("Severance S02E07 720p HDTV x264-GGEZ")
        );
        // REPACK folds in
        assert_eq!(
            content_key("Show.S01E01.REPACK.1080p.WEB-A"),
            content_key("Show.S01E01.1080p.WEB-B")
        );
        // different episodes differ
        assert_ne!(
            content_key("Show.S01E01.1080p.WEB"),
            content_key("Show.S01E02.1080p.WEB")
        );
        // events: quality variants fold
        assert_eq!(
            content_key("UFC.319.Main.Card.1080p.WEB.h264-VERUM"),
            content_key("UFC 319 Main Card 720p HDTV")
        );
        // non-Latin titles must keep their identity: two different shows'
        // S01E01 used to collapse onto one key and the second was never grabbed
        assert_ne!(
            content_key("進撃の巨人.S01E01.1080p.WEB"),
            content_key("鬼滅の刃.S01E01.1080p.WEB")
        );
        assert_eq!(
            content_key("Kızılcık.Şerbeti.S03E12.1080p.WEB"),
            content_key("Kizilcik Serbeti S03E12 720p HDTV")
        );
        // ASCII punctuation is dropped, not spaced: keys already stored in
        // users' ledgers ("its always sunny", "spiderman") must keep matching
        assert_eq!(content_key("It's.Always.Sunny.S16E03.1080p.WEB"), "tv:its always sunny:s16e03");
        assert_eq!(content_key("Spider-Man.2002.1080p.BluRay"), "item:spiderman:2002");
    }
}

// ============================ storage & runner ============================

use rusqlite::Connection;
use tauri::Manager;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BriefRow {
    pub id: i64,
    pub name: String,
    pub prompt: String,
    pub plan_json: String,
    pub cadence_minutes: i64,
    pub mode: String,
    pub max_grabs_per_run: i64,
    pub max_gb_per_run: f64,
    pub max_gb_per_day: f64,
    pub enabled: bool,
    pub created_at: i64,
    pub last_run_at: i64,
    pub last_report: Option<String>,
    pub fail_streak: i64,
    pub paused_reason: Option<String>,
}

pub fn list(conn: &Connection) -> Result<Vec<BriefRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, prompt, plan_json, cadence_minutes, mode, max_grabs_per_run,
                    max_gb_per_run, max_gb_per_day, enabled, created_at, last_run_at,
                    last_report, fail_streak, paused_reason
             FROM briefs ORDER BY name COLLATE NOCASE",
        )
        .map_err(crate::db::db_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(BriefRow {
                id: r.get(0)?,
                name: r.get(1)?,
                prompt: r.get(2)?,
                plan_json: r.get(3)?,
                cadence_minutes: r.get(4)?,
                mode: r.get(5)?,
                max_grabs_per_run: r.get(6)?,
                max_gb_per_run: r.get(7)?,
                max_gb_per_day: r.get(8)?,
                enabled: r.get::<_, i64>(9)? != 0,
                created_at: r.get(10)?,
                last_run_at: r.get(11)?,
                last_report: r.get(12)?,
                fail_streak: r.get(13)?,
                paused_reason: r.get(14)?,
            })
        })
        .map_err(crate::db::db_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(crate::db::db_err)?;
    Ok(rows)
}

pub const MIN_CADENCE_MINUTES: i64 = 15;

/// Execute one brief run end-to-end. Returns the report text.
pub async fn run_brief(app: &tauri::AppHandle, brief: &BriefRow) -> Result<String> {
    let state_guard = app.state::<crate::AppState>();
    let state: &crate::AppState = state_guard.inner();

    // anomaly breaker: a brief grabbing far beyond its shape gets auto-paused
    {
        let conn = state.db.lock().await;
        let today = crate::db::ledger_grabs_today(&conn, brief.id);
        let ceiling = (brief.max_grabs_per_run * 4).max(8);
        if today >= ceiling {
            let reason = format!("auto-paused: {today} grabs in 24h (ceiling {ceiling})");
            let _ = conn.execute(
                "UPDATE briefs SET enabled = 0, paused_reason = ?1 WHERE id = ?2",
                rusqlite::params![reason, brief.id],
            );
            crate::db::log_activity(&conn, "error", None, &format!("[brief: {}] {reason}", brief.name));
            return Err(AppError::Other(reason));
        }
    }

    // A plan that fails to parse must NEVER degrade to the unconstrained
    // default in auto mode — that's how a "1080p, ≤6GB" brief starts
    // grabbing 200GB packs. Pause loudly instead (empty = never a plan).
    let plan: HuntPlan = if brief.plan_json.trim().is_empty() {
        Default::default()
    } else {
        match serde_json::from_str(&brief.plan_json) {
            Ok(p) => p,
            Err(e) => {
                let reason = format!(
                    "stored plan failed to parse ({e}) — re-save the brief to regenerate it"
                );
                let conn = state.db.lock().await;
                let _ = conn.execute(
                    "UPDATE briefs SET enabled = 0, paused_reason = ?1 WHERE id = ?2",
                    rusqlite::params![reason, brief.id],
                );
                crate::db::log_activity(&conn, "error", None, &format!("[brief: {}] {reason}", brief.name));
                return Err(AppError::Other(reason));
            }
        }
    };
    let propose_only = brief.mode != "auto";
    // Auto mode with an empty include list would run constraint-free — the
    // RSS arm already refuses such briefs (see the sweep's plan filter);
    // a manual run must not be the loophole.
    if !propose_only && plan.include.is_empty() {
        let reason =
            "auto mode needs a compiled plan with at least one include term — re-save the brief"
                .to_string();
        let conn = state.db.lock().await;
        let _ = conn.execute(
            "UPDATE briefs SET enabled = 0, paused_reason = ?1 WHERE id = ?2",
            rusqlite::params![reason, brief.id],
        );
        crate::db::log_activity(&conn, "error", None, &format!("[brief: {}] {reason}", brief.name));
        return Err(AppError::Other(reason));
    }
    let memory = {
        let conn = state.db.lock().await;
        crate::db::memory_digest(&conn, brief.id)
    };
    let system = crate::agent_run::brief_system_prompt(
        &brief.name,
        &brief.prompt,
        &brief.plan_json,
        if memory.is_empty() { "(none yet)" } else { &memory },
        propose_only,
    );

    let mut ctx = crate::agent_tools::RunCtx::new(
        crate::agent_tools::RunOrigin::Brief { id: brief.id, name: brief.name.clone() },
        Some(plan),
        !propose_only,
    );
    ctx.max_grabs = brief.max_grabs_per_run.clamp(1, 10) as u32;
    ctx.max_gb = brief.max_gb_per_run.clamp(1.0, 200.0);

    let messages = vec![
        crate::llm::ChatMsg::system(system),
        crate::llm::ChatMsg::user("Run this brief now."),
    ];
    let run_id = format!("brief-{}-{}", brief.id, crate::db::now());

    let outcome = tokio::time::timeout(
        // a single LLM call may take up to 300s (llm.rs client timeout) —
        // the run deadline must exceed one slow call or slow models fail
        // forever and the fail-streak cool-off kicks in on non-failures
        std::time::Duration::from_secs(360),
        crate::agent_run::run(app, &mut ctx, messages, &run_id, false),
    )
    .await
    .map_err(|_| AppError::Other("brief run exceeded its 6-minute deadline".into()))??;

    let conn = state.db.lock().await;
    crate::db::log_activity(
        &conn,
        "agent",
        None,
        &format!(
            "[brief: {}] run finished — {} grabbed, {} proposed",
            brief.name, outcome.grabs, outcome.proposals
        ),
    );
    if outcome.grabs > 0 {
        let cfg = state.config.try_read().map(|c| c.notify_on_grab).unwrap_or(true);
        if cfg {
            use tauri_plugin_notification::NotificationExt;
            let _ = app
                .notification()
                .builder()
                .title(format!("Trawler brief: {}", brief.name))
                .body(format!("{} release(s) grabbed", outcome.grabs))
                .show();
        }
        crate::notify::dispatch(
            app,
            crate::notify::Kind::Grab,
            format!("Brief \u{201C}{}\u{201D} grabbed", brief.name),
            format!("{} release(s) grabbed", outcome.grabs),
        );
    }
    if outcome.new_proposals > 0 {
        crate::notify::dispatch(
            app,
            crate::notify::Kind::Proposal,
            format!("Brief \u{201C}{}\u{201D} has {} new proposal(s)", brief.name, outcome.new_proposals),
            "Waiting for your approval in the Agent view".into(),
        );
    }
    Ok(outcome.final_text)
}

/// Called every minute; runs due briefs sequentially. Overlap-guarded by caller.
pub async fn tick(app: &tauri::AppHandle) {
    let state_guard = app.state::<crate::AppState>();
    let state: &crate::AppState = state_guard.inner();
    let enabled = { state.config.read().await.agent_enabled };
    if !enabled {
        return;
    }
    let due: Vec<BriefRow> = {
        let conn = state.db.lock().await;
        match list(&conn) {
            Ok(all) => {
                let now = crate::db::now();
                all.into_iter()
                    .filter(|b| b.enabled)
                    .filter(|b| {
                        // circuit breaker: 3+ consecutive failures → 6h cool-off
                        if b.fail_streak >= 3 && now - b.last_run_at < 6 * 3600 {
                            return false;
                        }
                        now - b.last_run_at >= b.cadence_minutes.max(MIN_CADENCE_MINUTES) * 60
                    })
                    .collect()
            }
            Err(_) => vec![],
        }
    };

    for brief in due {
        let result = run_brief(app, &brief).await;
        let conn = state.db.lock().await;
        match result {
            Ok(report) => {
                let _ = conn.execute(
                    "UPDATE briefs SET last_run_at = ?1, last_report = ?2, fail_streak = 0, paused_reason = NULL
                     WHERE id = ?3",
                    rusqlite::params![crate::db::now(), report, brief.id],
                );
            }
            Err(e) => {
                let _ = conn.execute(
                    "UPDATE briefs SET last_run_at = ?1, fail_streak = fail_streak + 1 WHERE id = ?2",
                    rusqlite::params![crate::db::now(), brief.id],
                );
                crate::db::log_activity(
                    &conn,
                    "error",
                    None,
                    &format!("[brief: {}] run failed: {e}", brief.name),
                );
            }
        }
    }
}
