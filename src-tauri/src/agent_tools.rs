//! The agent's tool surface. Every hard guarantee lives HERE, in Rust —
//! never in the prompt. Models see opaque result ids, sanitized text, and
//! structured errors; magnets and budgets never enter model space.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::briefs::{content_key, HuntPlan};
use crate::commands::{perform_grab, perform_search};
use crate::db;
use crate::AppState;

#[derive(Debug, Clone)]
pub enum RunOrigin {
    Chat,
    Brief { id: i64, name: String },
    /// dead-swarm replacement runs
    Medic,
}

impl RunOrigin {
    pub fn label(&self) -> String {
        match self {
            RunOrigin::Chat => "chat".into(),
            RunOrigin::Brief { name, .. } => format!("brief: {name}"),
            RunOrigin::Medic => "medic".into(),
        }
    }
    pub fn brief_id(&self) -> Option<i64> {
        match self {
            RunOrigin::Brief { id, .. } => Some(*id),
            _ => None,
        }
    }
}

/// Full release data held server-side; the model only ever sees the id.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredResult {
    pub title: String,
    pub size: i64,
    pub seeders: Option<i32>,
    pub indexer: Option<String>,
    pub resolution: Option<String>,
    pub source: Option<String>,
    pub codec: Option<String>,
    pub season_pack: bool,
    pub content_key: String,
    #[serde(skip_serializing)]
    pub magnet_url: Option<String>,
    #[serde(skip_serializing)]
    pub download_url: Option<String>,
    pub info_hash: Option<String>,
}

pub struct RunCtx {
    pub origin: RunOrigin,
    pub plan: Option<HuntPlan>,
    /// propose-mode briefs cannot grab, only propose — enforced here, not by prompt
    pub allow_grab: bool,
    pub results: HashMap<String, StoredResult>,
    next_id: u32,
    pub searches_used: u32,
    pub grabs_done: u32,
    pub gb_done: f64,
    pub proposals_made: u32,
    /// proposals that were genuinely new (not refreshes of a pending card)
    pub new_proposals_made: u32,
    pub tool_calls_used: u32,
    seen_calls: HashSet<String>,
    pub grabbed_titles: Vec<String>,
    /// medic runs only: episodes the replacement grab should re-link
    pub medic_ep_ids: Vec<i64>,
    // caps
    pub max_searches: u32,
    pub max_grabs: u32,
    pub max_gb: f64,
    pub max_tool_calls: u32,
}

impl RunCtx {
    pub fn new(origin: RunOrigin, plan: Option<HuntPlan>, allow_grab: bool) -> Self {
        Self {
            origin,
            plan,
            allow_grab,
            results: HashMap::new(),
            next_id: 0,
            searches_used: 0,
            grabs_done: 0,
            gb_done: 0.0,
            proposals_made: 0,
            new_proposals_made: 0,
            tool_calls_used: 0,
            seen_calls: HashSet::new(),
            grabbed_titles: vec![],
            medic_ep_ids: vec![],
            max_searches: 8,
            max_grabs: 3,
            max_gb: 15.0,
            max_tool_calls: 12,
        }
    }

    fn store(&mut self, r: StoredResult) -> String {
        self.next_id += 1;
        let id = format!("r{}", self.next_id);
        self.results.insert(id.clone(), r);
        id
    }
}

/// Strip control chars, collapse whitespace, cap length — applied to every
/// string that originated on an indexer before it reaches model, DB, or UI.
pub fn clean_text(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| match c {
            c if c.is_control() => ' ',
            // angle brackets could forge/close the untrusted-data fence the
            // agent prompt relies on — neutralize to lookalikes
            '<' => '\u{2039}',
            '>' => '\u{203A}',
            c => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

fn err(msg: impl Into<String>) -> Value {
    json!({ "error": msg.into() })
}

pub fn tool_defs(ctx: &RunCtx) -> Value {
    let mut tools = vec![
        json!({"type":"function","function":{"name":"search_releases","description":"Search all torrent indexers. Returns up to 25 ranked releases, each with an opaque id used for grabbing/proposing. Costs one search from a limited budget.","parameters":{"type":"object","properties":{"query":{"type":"string","description":"scene-style search terms"},"kind":{"type":"string","enum":["all","movies","tv"]},"max_size_gb":{"type":"number"},"min_seeders":{"type":"integer"}},"required":["query"]}}}),
        json!({"type":"function","function":{"name":"propose_release","description":"Propose a release for the user's approval instead of grabbing it. Use the id from search_releases.","parameters":{"type":"object","properties":{"result_id":{"type":"string"},"reason":{"type":"string","description":"one short sentence"}},"required":["result_id","reason"]}}}),
        json!({"type":"function","function":{"name":"search_shows_tvmaze","description":"Search the TVmaze TV-show database (for follows, not for releases).","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}}),
        json!({"type":"function","function":{"name":"estimate_follow","description":"Estimate the cost of following a show (episode count, size). Following itself requires user confirmation in the UI.","parameters":{"type":"object","properties":{"tvmaze_id":{"type":"integer"},"backfill":{"type":"boolean"}},"required":["tvmaze_id"]}}}),
        json!({"type":"function","function":{"name":"list_follows","description":"List the shows the user follows, with progress.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"get_downloads","description":"Current downloads with progress and state.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"get_activity","description":"Recent app activity log entries.","parameters":{"type":"object","properties":{"limit":{"type":"integer"}},"required":[]}}}),
    ];
    if ctx.allow_grab {
        tools.push(json!({"type":"function","function":{"name":"grab_release","description":"Send a release to the download client. Use the id from search_releases. Subject to hard budgets; prefer the best single candidate over grabbing many.","parameters":{"type":"object","properties":{"result_id":{"type":"string"},"reason":{"type":"string","description":"one short sentence"}},"required":["result_id","reason"]}}}));
    }
    if matches!(ctx.origin, RunOrigin::Brief { .. }) {
        tools.push(json!({"type":"function","function":{"name":"remember","description":"Store one short fact for this brief's future runs (e.g. naming quirks discovered). NOT for tracking grabs — that is automatic.","parameters":{"type":"object","properties":{"key":{"type":"string"},"value":{"type":"string"}},"required":["key","value"]}}}));
    }
    Value::Array(tools)
}

/// Execute one tool call. Always returns a JSON value (errors are structured,
/// never panics) so the model can recover or conclude.
pub async fn execute(state: &AppState, ctx: &mut RunCtx, name: &str, args: &Value) -> Value {
    ctx.tool_calls_used += 1;
    if ctx.tool_calls_used > ctx.max_tool_calls {
        return err("tool budget exhausted — conclude now with what you have");
    }
    // exact-duplicate detection: same tool with same normalized args
    let dup_key = format!("{name}:{}", clean_text(&args.to_string(), 300).to_lowercase());
    if !matches!(name, "get_downloads" | "get_activity") && !ctx.seen_calls.insert(dup_key) {
        return err("you already made this exact call in this run — do something different or conclude");
    }

    match name {
        "search_releases" => search_releases(state, ctx, args).await,
        "grab_release" => grab_release(state, ctx, args).await,
        "propose_release" => propose_release(state, ctx, args).await,
        "search_shows_tvmaze" => search_shows(state, args).await,
        "estimate_follow" => estimate_follow(state, args).await,
        "list_follows" => list_follows(state).await,
        "get_downloads" => get_downloads(state).await,
        "get_activity" => get_activity(state, args).await,
        "remember" => remember(state, ctx, args).await,
        _ => err(format!("unknown tool {name}")),
    }
}

async fn search_releases(state: &AppState, ctx: &mut RunCtx, args: &Value) -> Value {
    if ctx.searches_used >= ctx.max_searches {
        return err("search budget exhausted — judge what you already found or conclude");
    }

    let query = match args.get("query").and_then(|q| q.as_str()) {
        Some(q) if !q.trim().is_empty() => clean_text(q, 80),
        _ => return err("query is required"),
    };
    let kind = args.get("kind").and_then(|k| k.as_str()).unwrap_or("all");
    let kind = if ["all", "movies", "tv"].contains(&kind) { kind } else { "all" };
    let max_size_gb = args.get("max_size_gb").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let min_seeders = args.get("min_seeders").and_then(|v| v.as_i64()).unwrap_or(1);

    let resp = match perform_search(state, &query, kind, &[]).await {
        Ok(r) => r,
        Err(e) => return err(format!("search failed: {e}")),
    };
    // only a search that actually reached the indexers costs budget — a broken
    // Prowlarr must not burn all 8 attempts and report "nothing new"
    ctx.searches_used += 1;

    // Same pipeline as manual search (parse/rank/relevance) — then the plan's
    // hard constraints, so the model never even sees out-of-scope releases.
    let mut rows = vec![];
    for r in resp.releases.iter().filter(|r| r.relevant) {
        let seeders = r.release.seeders;
        if seeders.map(|s| (s as i64) < min_seeders.max(1)).unwrap_or(false) {
            continue;
        }
        if max_size_gb > 0.0 && r.release.size > 0 && r.release.size as f64 > max_size_gb * 1e9 {
            continue;
        }
        if let Some(plan) = &ctx.plan {
            if plan.allows(&r.release.title, r.release.size, seeders).is_err() {
                continue;
            }
        }
        let stored = StoredResult {
            title: clean_text(&r.release.title, 140),
            size: r.release.size,
            seeders,
            indexer: r.release.indexer.clone().map(|i| clean_text(&i, 40)),
            resolution: r.parsed.resolution.clone(),
            source: r.parsed.source.clone(),
            codec: r.parsed.codec.clone(),
            season_pack: r.parsed.season_pack,
            content_key: content_key(&r.release.title),
            magnet_url: r.release.magnet_url.clone(),
            download_url: r.release.download_url.clone(),
            info_hash: r.release.info_hash.clone(),
        };
        let already = {
            let conn = state.db.lock().await;
            db::ledger_satisfied(&conn, &stored.content_key)
        };
        let id = ctx.store(stored);
        let s = &ctx.results[&id];
        rows.push(json!({
            "id": id,
            "title": s.title,
            "sizeGb": (s.size as f64 / 1e9 * 10.0).round() / 10.0,
            "seeders": s.seeders,
            "indexer": s.indexer,
            "resolution": s.resolution,
            "source": s.source,
            "codec": s.codec,
            "seasonPack": s.season_pack,
            "alreadyHave": already,
        }));
        if rows.len() >= 25 {
            break;
        }
    }
    json!({
        "query": query,
        "results": rows,
        "searchesRemaining": ctx.max_searches - ctx.searches_used,
    })
}

async fn grab_release(state: &AppState, ctx: &mut RunCtx, args: &Value) -> Value {
    if !ctx.allow_grab {
        return err("this run is propose-only — use propose_release instead");
    }
    let id = match args.get("result_id").and_then(|v| v.as_str()) {
        Some(i) => i.to_string(),
        None => return err("result_id is required"),
    };
    // Provenance: only results returned by a search in THIS run are grabbable.
    let stored = match ctx.results.get(&id) {
        Some(s) => s.clone(),
        None => return err(format!("unknown result_id {id} — ids come from search_releases in this run")),
    };
    if ctx.grabs_done >= ctx.max_grabs {
        return err("grab budget exhausted for this run");
    }
    let size_gb = stored.size as f64 / 1e9;
    if ctx.gb_done + size_gb > ctx.max_gb {
        return err(format!(
            "would exceed this run's {:.0} GB budget ({:.1} used, item is {size_gb:.1})",
            ctx.max_gb, ctx.gb_done
        ));
    }
    // Compiled-plan re-check at the moment of truth.
    if let Some(plan) = &ctx.plan {
        if let Err(reason) = plan.allows(&stored.title, stored.size, stored.seeders) {
            return err(format!("blocked by the brief's plan: {reason}"));
        }
    }
    // Semantic dedupe + rolling budgets, straight from the shared ledger.
    {
        let conn = state.db.lock().await;
        if db::ledger_satisfied(&conn, &stored.content_key) {
            return err("already grabbed this content (possibly a different release of it)");
        }
        if let Some(brief_id) = ctx.origin.brief_id() {
            let cfg_max_day = conn
                .query_row("SELECT max_gb_per_day FROM briefs WHERE id = ?1", [brief_id], |r| {
                    r.get::<_, f64>(0)
                })
                .unwrap_or(30.0);
            if db::ledger_gb_today(&conn, brief_id) + size_gb > cfg_max_day {
                return err(format!("brief's {cfg_max_day:.0} GB daily budget exhausted"));
            }
        }
    }
    // Free-disk floor from qBittorrent's own accounting (best effort).
    let cfg = state.config.read().await.clone();
    let q = crate::qbit::QbitClient {
        http: &state.http,
        base: cfg.qbit_url.clone(),
        username: cfg.qbit_username.clone(),
        password: cfg.qbit_password.clone(),
    };
    if let Ok(free) = q.free_space().await {
        if (free as f64) < cfg.agent_min_free_disk_gb * 1e9 {
            return err(format!(
                "refusing: download disk has only {:.0} GB free (floor is {:.0} GB)",
                free as f64 / 1e9,
                cfg.agent_min_free_disk_gb
            ));
        }
    }

    let reason = clean_text(args.get("reason").and_then(|v| v.as_str()).unwrap_or(""), 200);
    match perform_grab(state, &stored.title, stored.magnet_url.clone(), stored.download_url.clone(), None).await {
        Ok(_) => {
            ctx.grabs_done += 1;
            ctx.gb_done += size_gb;
            ctx.grabbed_titles.push(stored.title.clone());
            let conn = state.db.lock().await;
            db::ledger_insert(
                &conn,
                &stored.content_key,
                ctx.origin.brief_id(),
                &stored.title,
                stored.info_hash.as_deref(),
                stored.size,
                &ctx.medic_ep_ids,
            );
            if !ctx.medic_ep_ids.is_empty() {
                // the dead grab freed these to wanted — the replacement owns
                // them now, or they'd re-search forever and never complete
                db::mark_grabbed(&conn, &ctx.medic_ep_ids, &stored.title);
            }
            db::log_activity(
                &conn,
                "agent",
                None,
                &format!("[{}] grabbed {} · {:.1} GB · {}", ctx.origin.label(), stored.title, size_gb, reason),
            );
            json!({ "ok": true, "grabbed": stored.title, "grabsRemaining": ctx.max_grabs - ctx.grabs_done })
        }
        Err(e) => err(format!("grab failed: {e}")),
    }
}

async fn propose_release(state: &AppState, ctx: &mut RunCtx, args: &Value) -> Value {
    let id = match args.get("result_id").and_then(|v| v.as_str()) {
        Some(i) => i.to_string(),
        None => return err("result_id is required"),
    };
    let stored = match ctx.results.get(&id) {
        Some(s) => s.clone(),
        None => return err(format!("unknown result_id {id}")),
    };
    if ctx.proposals_made >= 6 {
        return err("proposal budget exhausted — conclude");
    }
    // A proposal must also satisfy the plan: a card the user cannot approve is noise.
    if let Some(plan) = &ctx.plan {
        if let Err(reason) = plan.allows(&stored.title, stored.size, stored.seeders) {
            return err(format!("does not satisfy the brief's plan: {reason}"));
        }
    }
    let reason = clean_text(args.get("reason").and_then(|v| v.as_str()).unwrap_or(""), 200);
    let result_json = serde_json::to_string(&json!({
        "title": stored.title,
        "size": stored.size,
        "seeders": stored.seeders,
        "indexer": stored.indexer,
        "resolution": stored.resolution,
        "source": stored.source,
        "codec": stored.codec,
        "magnetUrl": stored.magnet_url,
        "downloadUrl": stored.download_url,
        "infoHash": stored.info_hash,
    }))
    .unwrap_or_default();
    let is_new = {
        let conn = state.db.lock().await;
        if db::ledger_satisfied(&conn, &stored.content_key) {
            return err("already grabbed this content — no proposal needed");
        }
        db::proposal_upsert(&conn, ctx.origin.brief_id(), &stored.content_key, &result_json, &reason)
    };
    ctx.proposals_made += 1;
    if is_new {
        ctx.new_proposals_made += 1;
    }
    json!({ "ok": true, "proposed": stored.title })
}

async fn search_shows(state: &AppState, args: &Value) -> Value {
    let query = match args.get("query").and_then(|q| q.as_str()) {
        Some(q) if !q.trim().is_empty() => q,
        _ => return err("query is required"),
    };
    match crate::tvmaze::search_shows(&state.http, query).await {
        Ok(shows) => json!(shows
            .iter()
            .take(8)
            .map(|s| json!({
                "tvmazeId": s.id,
                "name": s.name,
                "status": s.status,
                "year": s.premiered.as_deref().map(|p| p.get(..4).unwrap_or("")),
                "network": s.network_name(),
            }))
            .collect::<Vec<_>>()),
        Err(e) => err(format!("tvmaze search failed: {e}")),
    }
}

async fn estimate_follow(state: &AppState, args: &Value) -> Value {
    let id = match args.get("tvmaze_id").and_then(|v| v.as_i64()) {
        Some(i) => i,
        None => return err("tvmaze_id is required"),
    };
    let backfill = args.get("backfill").and_then(|v| v.as_bool()).unwrap_or(true);
    match crate::tvmaze::show_with_episodes(&state.http, id).await {
        Ok(show) => {
            let eps = show.embedded.as_ref().map(|e| e.episodes.len()).unwrap_or(0);
            json!({
                "name": show.name,
                "status": show.status,
                "episodes": eps,
                "estimatedGb": if backfill { (eps as f64 * 1.5).round() } else { 0.0 },
                "note": "following requires the user to confirm in the app — tell them the estimate and let them decide",
            })
        }
        Err(e) => err(format!("tvmaze lookup failed: {e}")),
    }
}

async fn list_follows(state: &AppState) -> Value {
    let conn = state.db.lock().await;
    match db::list_shows(&conn) {
        Ok(shows) => json!(shows
            .iter()
            .map(|s| json!({
                "tvmazeId": s.tvmaze_id,
                "name": s.name,
                "status": s.status,
                "downloaded": s.downloaded,
                "total": s.total,
                "wanted": s.wanted,
            }))
            .collect::<Vec<_>>()),
        Err(e) => err(e.to_string()),
    }
}

async fn get_downloads(state: &AppState) -> Value {
    let cfg = state.config.read().await.clone();
    let q = crate::qbit::QbitClient {
        http: &state.http,
        base: cfg.qbit_url.clone(),
        username: cfg.qbit_username.clone(),
        password: cfg.qbit_password.clone(),
    };
    match q.list(None).await {
        Ok(ts) => json!(ts
            .iter()
            .take(20)
            .map(|t| json!({
                "name": clean_text(&t.name, 120),
                "progressPct": (t.progress * 100.0).round(),
                "state": t.state,
                "sizeGb": (t.size as f64 / 1e9 * 10.0).round() / 10.0,
                "dlSpeedMbps": (t.dlspeed as f64 / 1e6 * 10.0).round() / 10.0,
                "seeds": t.num_seeds,
            }))
            .collect::<Vec<_>>()),
        Err(e) => err(format!("qBittorrent unreachable: {e}")),
    }
}

async fn get_activity(state: &AppState, args: &Value) -> Value {
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(15).clamp(1, 30);
    let conn = state.db.lock().await;
    match db::list_activity(&conn, limit) {
        Ok(rows) => json!(rows
            .iter()
            .map(|a| json!({ "kind": a.kind, "message": clean_text(&a.message, 200) }))
            .collect::<Vec<_>>()),
        Err(e) => err(e.to_string()),
    }
}

async fn remember(state: &AppState, ctx: &mut RunCtx, args: &Value) -> Value {
    let brief_id = match ctx.origin.brief_id() {
        Some(id) => id,
        None => return err("memory is only available to brief runs"),
    };
    let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("");
    let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
    if key.trim().is_empty() || value.trim().is_empty() {
        return err("key and value are required");
    }
    let conn = state.db.lock().await;
    db::memory_put(&conn, brief_id, &clean_text(key, 60), &clean_text(value, 300));
    json!({ "ok": true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_neutralizes() {
        assert_eq!(clean_text("a\u{0}b\r\nc   d", 100), "a b c d");
        assert_eq!(clean_text(&"x".repeat(500), 10).len(), 10);
    }

    #[test]
    fn run_ctx_budgets() {
        let mut ctx = RunCtx::new(RunOrigin::Chat, None, true);
        ctx.max_tool_calls = 2;
        // budget counting is exercised through execute(); here verify the store/lookup contract
        let id = ctx.store(StoredResult {
            title: "T".into(),
            size: 1,
            seeders: Some(1),
            indexer: None,
            resolution: None,
            source: None,
            codec: None,
            season_pack: false,
            content_key: "item:t".into(),
            magnet_url: None,
            download_url: None,
            info_hash: None,
        });
        assert_eq!(id, "r1");
        assert!(ctx.results.contains_key("r1"));
        assert!(!ctx.results.contains_key("r2"));
    }
}
