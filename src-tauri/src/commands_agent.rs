//! Tauri commands for the agent: chat, briefs, proposals, models.

use serde::Serialize;
use serde_json::Value;
use tauri::State;

use crate::agent_tools::{clean_text, RunCtx, RunOrigin};
use crate::briefs::{self, BriefRow, HuntPlan};
use crate::db;
use crate::error::{AppError, Result};
use crate::llm::{ChatMsg, LlmClient};
use crate::AppState;

// ---------- chat ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRow {
    pub id: i64,
    pub ts: i64,
    pub role: String,
    pub content: Option<String>,
    pub tool_name: Option<String>,
    pub tool_payload: Option<String>,
}

#[tauri::command]
pub async fn agent_history(state: State<'_, AppState>) -> Result<Vec<ChatRow>> {
    let conn = state.db.lock().await;
    let mut stmt = conn
        .prepare("SELECT id, ts, role, content, tool_name, tool_payload FROM chat_messages ORDER BY id DESC LIMIT 400")
        .map_err(db::db_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ChatRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                role: r.get(2)?,
                content: r.get(3)?,
                tool_name: r.get(4)?,
                tool_payload: r.get(5)?,
            })
        })
        .map_err(db::db_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db::db_err)?;
    Ok(rows)
}

#[tauri::command]
pub async fn agent_clear(state: State<'_, AppState>) -> Result<()> {
    let conn = state.db.lock().await;
    conn.execute("DELETE FROM chat_messages", []).map_err(db::db_err)?;
    Ok(())
}

/// Send a chat message. Returns immediately; progress streams via agent-step events.
#[tauri::command]
pub async fn agent_send(app: tauri::AppHandle, state: State<'_, AppState>, text: String) -> Result<String> {
    use std::sync::atomic::Ordering;
    let cfg = state.config.read().await.clone();
    if !cfg.agent_enabled {
        return Err(AppError::Other("the agent is disabled in Settings".into()));
    }
    if state.agent_chat_busy.swap(true, Ordering::SeqCst) {
        return Err(AppError::Other("the agent is still working on the previous message".into()));
    }

    let text = text.trim().chars().take(4000).collect::<String>();
    if text.is_empty() {
        state.agent_chat_busy.store(false, Ordering::SeqCst);
        return Err(AppError::Other("empty message".into()));
    }

    // persist the user message + build recent conversational context
    let history: Vec<(String, String)> = {
        let conn = state.db.lock().await;
        conn.execute(
            "INSERT INTO chat_messages (ts, role, content) VALUES (?1, 'user', ?2)",
            rusqlite::params![db::now(), text],
        )
        .map_err(db::db_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT role, content FROM chat_messages
                 WHERE role IN ('user','assistant') AND content IS NOT NULL
                 ORDER BY id DESC LIMIT 16",
            )
            .map_err(db::db_err)?;
        let mut rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(db::db_err)?
            .flatten()
            .collect();
        rows.reverse();
        rows
    };

    let mut messages = vec![ChatMsg::system(crate::agent_run::chat_system_prompt())];
    for (role, content) in history {
        messages.push(if role == "user" {
            ChatMsg::user(content)
        } else {
            ChatMsg::assistant_text(content)
        });
    }

    let run_id = format!("chat-{}", db::now());
    let run_id_out = run_id.clone();
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = tauri::Manager::state::<AppState>(&app2);
        let mut ctx = RunCtx::new(RunOrigin::Chat, None, true);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            crate::agent_run::run(&app2, &mut ctx, messages, &run_id, true),
        )
        .await;
        if let Err(_) | Ok(Err(_)) = &result {
            let msg = match result {
                Err(_) => "the run exceeded its 5-minute deadline".to_string(),
                Ok(Err(e)) => e.to_string(),
                _ => unreachable!(),
            };
            use tauri::Emitter;
            let _ = app2.emit(
                "agent-step",
                serde_json::json!({ "runId": run_id, "kind": "error", "payload": { "message": msg } }),
            );
            let conn = state.db.lock().await;
            let _ = conn.execute(
                "INSERT INTO chat_messages (ts, role, content) VALUES (?1, 'assistant', ?2)",
                rusqlite::params![db::now(), format!("⚠ {msg}")],
            );
        }
        state.agent_chat_busy.store(false, std::sync::atomic::Ordering::SeqCst);
    });

    Ok(run_id_out)
}

#[tauri::command]
pub async fn agent_models(state: State<'_, AppState>) -> Result<Vec<String>> {
    let cfg = state.config.read().await.clone();
    LlmClient::new(&cfg.agent_base_url, &cfg.agent_model).models().await
}

// ---------- briefs ----------

#[tauri::command]
pub async fn briefs_list(state: State<'_, AppState>) -> Result<Vec<BriefRow>> {
    let conn = state.db.lock().await;
    briefs::list(&conn)
}

/// Compile a natural-language brief into a HuntPlan for the user to confirm.
#[tauri::command]
pub async fn compile_brief(state: State<'_, AppState>, prompt: String) -> Result<HuntPlan> {
    let cfg = state.config.read().await.clone();
    if !cfg.agent_enabled {
        return Err(AppError::Other("the agent is disabled in Settings".into()));
    }
    let client = LlmClient::new(&cfg.agent_base_url, &cfg.agent_model);
    briefs::compile(&client, &prompt).await
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn brief_save(
    state: State<'_, AppState>,
    id: Option<i64>,
    name: String,
    prompt: String,
    plan: HuntPlan,
    cadence_minutes: i64,
    mode: String,
    max_grabs_per_run: i64,
    max_gb_per_run: f64,
    max_gb_per_day: f64,
    enabled: bool,
) -> Result<i64> {
    let plan = plan.sanitize();
    if plan.queries.is_empty() {
        return Err(AppError::Other("the plan needs at least one search query".into()));
    }
    let name = clean_text(&name, 60);
    if name.is_empty() {
        return Err(AppError::Other("the brief needs a name".into()));
    }
    let mode = if mode == "auto" { "auto" } else { "propose" };
    let cadence = cadence_minutes.max(briefs::MIN_CADENCE_MINUTES);
    let plan_json = serde_json::to_string(&plan)?;
    let conn = state.db.lock().await;
    match id {
        Some(id) => {
            conn.execute(
                "UPDATE briefs SET name=?1, prompt=?2, plan_json=?3, cadence_minutes=?4, mode=?5,
                        max_grabs_per_run=?6, max_gb_per_run=?7, max_gb_per_day=?8, enabled=?9,
                        paused_reason = CASE WHEN ?9 = 1 THEN NULL ELSE paused_reason END,
                        fail_streak = 0
                 WHERE id=?10",
                rusqlite::params![
                    name, prompt, plan_json, cadence, mode,
                    max_grabs_per_run.clamp(1, 10), max_gb_per_run.clamp(1.0, 200.0),
                    max_gb_per_day.clamp(1.0, 500.0), enabled as i64, id
                ],
            )
            .map_err(db::db_err)?;
            db::log_activity(&conn, "agent", None, &format!("Brief \"{name}\" updated"));
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO briefs (name, prompt, plan_json, cadence_minutes, mode,
                        max_grabs_per_run, max_gb_per_run, max_gb_per_day, enabled, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    name, prompt, plan_json, cadence, mode,
                    max_grabs_per_run.clamp(1, 10), max_gb_per_run.clamp(1.0, 200.0),
                    max_gb_per_day.clamp(1.0, 500.0), enabled as i64, db::now()
                ],
            )
            .map_err(db::db_err)?;
            db::log_activity(&conn, "agent", None, &format!("Brief \"{name}\" created ({mode} mode)"));
            Ok(conn.last_insert_rowid())
        }
    }
}

#[tauri::command]
pub async fn brief_delete(state: State<'_, AppState>, id: i64) -> Result<()> {
    let conn = state.db.lock().await;
    conn.execute("DELETE FROM briefs WHERE id = ?1", [id]).map_err(db::db_err)?;
    Ok(())
}

#[tauri::command]
pub async fn brief_run_now(app: tauri::AppHandle, state: State<'_, AppState>, id: i64) -> Result<()> {
    let brief = {
        let conn = state.db.lock().await;
        briefs::list(&conn)?
            .into_iter()
            .find(|b| b.id == id)
            .ok_or_else(|| AppError::Other("brief not found".into()))?
    };
    if !brief.enabled {
        return Err(AppError::Other("this brief is paused — enable it first".into()));
    }
    // share the tick's mutual exclusion so a manual run can't overlap the
    // scheduled run of the same brief (double grabs, interleaved reports)
    if state.brief_tick_busy.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err(AppError::Other("a brief run is already in progress — try again in a moment".into()));
    }
    tauri::async_runtime::spawn(async move {
        let state = tauri::Manager::state::<AppState>(&app);
        let result = briefs::run_brief(&app, &brief).await;
        state.brief_tick_busy.store(false, std::sync::atomic::Ordering::SeqCst);
        let conn = state.db.lock().await;
        match result {
            Ok(report) => {
                let _ = conn.execute(
                    "UPDATE briefs SET last_run_at = ?1, last_report = ?2, fail_streak = 0 WHERE id = ?3",
                    rusqlite::params![db::now(), report, brief.id],
                );
            }
            Err(e) => {
                let _ = conn.execute(
                    "UPDATE briefs SET last_run_at = ?1, fail_streak = fail_streak + 1 WHERE id = ?2",
                    rusqlite::params![db::now(), brief.id],
                );
                db::log_activity(&conn, "error", None, &format!("[brief: {}] manual run failed: {e}", brief.name));
            }
        }
    });
    Ok(())
}

// ---------- proposals ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalRow {
    pub id: i64,
    pub brief_id: Option<i64>,
    pub brief_name: Option<String>,
    pub content_key: String,
    pub result: Value,
    pub reason: Option<String>,
    pub status: String,
    pub first_seen: i64,
    pub last_seen: i64,
}

#[tauri::command]
pub async fn proposals_list(state: State<'_, AppState>) -> Result<Vec<ProposalRow>> {
    let conn = state.db.lock().await;
    let mut stmt = conn
        .prepare(
            "SELECT p.id, p.brief_id, b.name, p.content_key, p.result_json, p.reason, p.status, p.first_seen, p.last_seen
             FROM proposals p LEFT JOIN briefs b ON b.id = p.brief_id
             WHERE p.status = 'pending' ORDER BY p.last_seen DESC LIMIT 50",
        )
        .map_err(db::db_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ProposalRow {
                id: r.get(0)?,
                brief_id: r.get(1)?,
                brief_name: r.get(2)?,
                content_key: r.get(3)?,
                result: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or(Value::Null),
                reason: r.get(5)?,
                status: r.get(6)?,
                first_seen: r.get(7)?,
                last_seen: r.get(8)?,
            })
        })
        .map_err(db::db_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db::db_err)?;
    Ok(rows)
}

#[tauri::command]
pub async fn proposal_resolve(state: State<'_, AppState>, id: i64, approve: bool) -> Result<String> {
    let (content_key, result_json, brief_id) = {
        let conn = state.db.lock().await;
        conn.query_row(
            "SELECT content_key, result_json, brief_id FROM proposals WHERE id = ?1 AND status = 'pending'",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?)),
        )
        .map_err(|_| AppError::Other("proposal not found or already resolved".into()))?
    };

    if !approve {
        let conn = state.db.lock().await;
        // refresh last_seen so the re-proposal cooldown counts from the
        // dismissal, not from when the card was filed
        conn.execute(
            "UPDATE proposals SET status = 'dismissed', last_seen = ?2 WHERE id = ?1",
            rusqlite::params![id, db::now()],
        )
        .map_err(db::db_err)?;
        return Ok("dismissed".into());
    }

    let v: Value = serde_json::from_str(&result_json)?;
    let title = v.get("title").and_then(|t| t.as_str()).unwrap_or("?").to_string();
    let magnet = v.get("magnetUrl").and_then(|m| m.as_str()).map(String::from);
    let download = v.get("downloadUrl").and_then(|d| d.as_str()).map(String::from);
    let size = v.get("size").and_then(|s| s.as_i64()).unwrap_or(0);
    let info_hash = v.get("infoHash").and_then(|h| h.as_str()).map(String::from);
    // the upgrade scout resolves the show's save path at propose time so the
    // approved file lands in the library, not qBittorrent's default folder
    let save_path = v
        .get("savePath")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    crate::commands::perform_grab(state.inner(), &title, magnet, download, save_path).await?;

    let conn = state.db.lock().await;
    conn.execute("UPDATE proposals SET status = 'approved' WHERE id = ?1", [id])
        .map_err(db::db_err)?;
    db::ledger_insert(&conn, &content_key, brief_id, &title, info_hash.as_deref(), size, &[]);
    db::log_activity(&conn, "agent", None, &format!("Approved proposal: {title}"));
    Ok(format!("Grabbed {title}"))
}
