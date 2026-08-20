//! SQLite persistence for followed shows, episodes and activity.
//! One connection behind a Mutex — every helper is synchronous and quick;
//! never hold the lock across an await.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use crate::error::{AppError, Result};

pub fn open() -> Result<Connection> {
    let path = crate::config::config_path()
        .parent()
        .map(|d| d.join("trawler.db"))
        .ok_or_else(|| AppError::Other("cannot resolve data dir".into()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(&path).map_err(db_err)?;
    // background tasks open their own connections (see open_existing);
    // WAL serializes writers — the timeout makes them wait, not error
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(db_err)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;

         CREATE TABLE IF NOT EXISTS shows (
           tvmaze_id   INTEGER PRIMARY KEY,
           name        TEXT NOT NULL,
           status      TEXT NOT NULL,
           poster_url  TEXT,
           premiered   TEXT,
           ended       TEXT,
           network     TEXT,
           imdb_id     TEXT,
           followed_at INTEGER NOT NULL,
           refreshed_at INTEGER NOT NULL DEFAULT 0,
           quality_json TEXT,
           save_path_override TEXT,
           backfill    INTEGER NOT NULL DEFAULT 1
         );

         CREATE TABLE IF NOT EXISTS episodes (
           tvmaze_ep_id INTEGER PRIMARY KEY,
           show_id      INTEGER NOT NULL REFERENCES shows(tvmaze_id) ON DELETE CASCADE,
           season       INTEGER NOT NULL,
           number       INTEGER NOT NULL,
           title        TEXT,
           airstamp     TEXT,
           state        TEXT NOT NULL DEFAULT 'upcoming',
           grabbed_title TEXT,
           grabbed_at   INTEGER
         );
         CREATE INDEX IF NOT EXISTS idx_episodes_show ON episodes(show_id, season, number);
         CREATE INDEX IF NOT EXISTS idx_episodes_state ON episodes(state);

         CREATE TABLE IF NOT EXISTS activity (
           id      INTEGER PRIMARY KEY AUTOINCREMENT,
           ts      INTEGER NOT NULL,
           kind    TEXT NOT NULL,
           show_id INTEGER,
           message TEXT NOT NULL
         );",
    )
    .map_err(db_err)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS briefs (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           name TEXT NOT NULL,
           prompt TEXT NOT NULL,
           plan_json TEXT NOT NULL,
           cadence_minutes INTEGER NOT NULL DEFAULT 60,
           mode TEXT NOT NULL DEFAULT 'propose',
           max_grabs_per_run INTEGER NOT NULL DEFAULT 3,
           max_gb_per_run REAL NOT NULL DEFAULT 15,
           max_gb_per_day REAL NOT NULL DEFAULT 30,
           enabled INTEGER NOT NULL DEFAULT 1,
           created_at INTEGER NOT NULL,
           last_run_at INTEGER NOT NULL DEFAULT 0,
           last_report TEXT,
           fail_streak INTEGER NOT NULL DEFAULT 0,
           paused_reason TEXT
         );

         CREATE TABLE IF NOT EXISTS grab_ledger (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           content_key TEXT NOT NULL,
           brief_id INTEGER,
           title TEXT NOT NULL,
           info_hash TEXT,
           size INTEGER NOT NULL DEFAULT 0,
           state TEXT NOT NULL DEFAULT 'grabbed',
           ts INTEGER NOT NULL,
           ep_ids TEXT,
           backend TEXT NOT NULL DEFAULT 'qbittorrent',
           bp_token TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_ledger_key ON grab_ledger(content_key);

         CREATE TABLE IF NOT EXISTS brief_memory (
           brief_id INTEGER NOT NULL,
           key TEXT NOT NULL,
           value TEXT NOT NULL,
           ts INTEGER NOT NULL,
           PRIMARY KEY (brief_id, key)
         );

         CREATE TABLE IF NOT EXISTS proposals (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           brief_id INTEGER,
           content_key TEXT NOT NULL,
           result_json TEXT NOT NULL,
           reason TEXT,
           status TEXT NOT NULL DEFAULT 'pending',
           first_seen INTEGER NOT NULL,
           last_seen INTEGER NOT NULL
         );
         DELETE FROM proposals WHERE status = 'pending' AND id NOT IN (
           SELECT MAX(id) FROM proposals WHERE status = 'pending' GROUP BY content_key
         );
         CREATE UNIQUE INDEX IF NOT EXISTS idx_proposals_key
           ON proposals(content_key) WHERE status = 'pending';
         CREATE INDEX IF NOT EXISTS idx_proposals_ck_status
           ON proposals(content_key, status);

         CREATE TABLE IF NOT EXISTS meta (
           key TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );

         CREATE TABLE IF NOT EXISTS chat_messages (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           ts INTEGER NOT NULL,
           role TEXT NOT NULL,
           content TEXT,
           tool_name TEXT,
           tool_payload TEXT
         );",
    )
    .map_err(db_err)?;

    // additive migrations — ignore "duplicate column" on existing databases
    let _ = conn.execute(
        "ALTER TABLE episodes ADD COLUMN last_searched_at INTEGER NOT NULL DEFAULT 0",
        [],
    );
    let _ = conn.execute("ALTER TABLE shows ADD COLUMN search_alias TEXT", []);
    // which episodes a grab belongs to, as a JSON id array — durable linkage
    // that survives unfollow/refollow (grabbed_title matching does not)
    let _ = conn.execute(
        "ALTER TABLE grab_ledger ADD COLUMN backend TEXT NOT NULL DEFAULT 'qbittorrent'",
        [],
    );
    let _ = conn.execute("ALTER TABLE grab_ledger ADD COLUMN bp_token TEXT", []);
    if let Err(e) = conn.execute("ALTER TABLE grab_ledger ADD COLUMN ep_ids TEXT", []) {
        let msg = e.to_string();
        if !msg.contains("duplicate column") {
            crate::applog::warn("app",format!("ep_ids migration failed: {msg}"));
        }
    }
    // one-shot backfill: link every healthy in-flight grab to its episodes by
    // the title match that still works TODAY, so the linkage survives the
    // refollow that would otherwise sever it tomorrow
    let backfilled: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'ledger_ep_backfill'", [], |r| r.get(0))
        .ok();
    if backfilled.is_none() {
        let rows: Vec<(i64, String)> = conn
            .prepare("SELECT id, title FROM grab_ledger WHERE state IN ('grabbed','completed') AND ep_ids IS NULL")
            .ok()
            .map(|mut stmt| {
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map(|it| it.flatten().collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let eps: Vec<(i64, String)> = conn
            .prepare("SELECT tvmaze_ep_id, grabbed_title FROM episodes WHERE grabbed_title IS NOT NULL")
            .ok()
            .map(|mut stmt| {
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map(|it| it.flatten().collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        for (id, title) in rows {
            let norm = crate::commands::normalize(&title);
            let linked: Vec<i64> = eps
                .iter()
                .filter(|(_, gt)| crate::commands::normalize(gt) == norm)
                .map(|(e, _)| *e)
                .collect();
            if !linked.is_empty() {
                if let Ok(json) = serde_json::to_string(&linked) {
                    let _ = conn.execute(
                        "UPDATE grab_ledger SET ep_ids = ?2 WHERE id = ?1",
                        rusqlite::params![id, json],
                    );
                }
            }
        }
        let _ = conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('ledger_ep_backfill', '1')",
            [],
        );
    }
    let _ = conn.execute(
        "ALTER TABLE shows ADD COLUMN alias_status TEXT",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE shows ADD COLUMN seasons_json TEXT",
        [],
    );
    Ok(conn)
}

/// A second connection for short-lived background writes (grab recording).
/// The app's main connection owns schema setup — this one must not migrate.
pub fn open_existing() -> Result<Connection> {
    let path = crate::config::config_path()
        .parent()
        .map(|d| d.join("trawler.db"))
        .ok_or_else(|| AppError::Other("cannot resolve data dir".into()))?;
    let conn = Connection::open(&path).map_err(db_err)?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(db_err)?;
    Ok(conn)
}

// ---------- grab ledger (shared: scheduler + agent) ----------

/// Is this content already satisfied (grabbed or completed)?
pub fn ledger_satisfied(conn: &Connection, content_key: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM grab_ledger WHERE content_key = ?1 AND state IN ('grabbed','completed')",
        [content_key],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

pub fn ledger_insert(
    conn: &Connection,
    content_key: &str,
    brief_id: Option<i64>,
    title: &str,
    info_hash: Option<&str>,
    size: i64,
    ep_ids: &[i64],
    backend: &str,
    bp_token: Option<&str>,
) -> Result<()> {
    let eps_json = if ep_ids.is_empty() {
        None
    } else {
        serde_json::to_string(ep_ids).ok()
    };
    conn.execute(
        "INSERT INTO grab_ledger (content_key, brief_id, title, info_hash, size, state, ts, ep_ids, backend, bp_token)
         VALUES (?1, ?2, ?3, ?4, ?5, 'grabbed', ?6, ?7, ?8, ?9)",
        rusqlite::params![content_key, brief_id, title, info_hash, size, now(), eps_json, backend, bp_token],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Parse a ledger row's ep_ids JSON into ids (empty when absent/invalid).
pub fn parse_ep_ids(raw: Option<&str>) -> Vec<i64> {
    raw.and_then(|r| serde_json::from_str::<Vec<i64>>(r).ok()).unwrap_or_default()
}

/// Keep chat history bounded — every turn (and every tool result's JSON)
/// is persisted, and nothing else ever prunes it. Reads cap at 400 rows;
/// old ones are pure dead weight in the DB.
pub fn prune_chat(conn: &Connection) {
    let _ = conn.execute(
        "DELETE FROM chat_messages WHERE id <= (
           SELECT id FROM chat_messages ORDER BY id DESC LIMIT 1 OFFSET 2000
         )",
        [],
    );
}

/// Dismissed proposal cards accumulate forever otherwise — the 7-day window
/// matches the re-proposal cooldown they gate.
pub fn prune_dismissed_proposals(conn: &Connection) {
    let _ = conn.execute(
        "DELETE FROM proposals WHERE status = 'dismissed' AND last_seen < ?1",
        [now() - 7 * 86_400],
    );
}

/// Flip a grab's linked episodes to a new state by id — the linkage that
/// keeps working after unfollow/refollow wipes grabbed_title.
pub fn set_episodes_state_by_ids(conn: &Connection, ids: &[i64], state: &str, grabbed_title: Option<&str>) {
    if ids.is_empty() {
        return;
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    match (state, grabbed_title) {
        ("downloaded", _) => {
            let sql = format!(
                "UPDATE episodes SET state = 'downloaded' WHERE tvmaze_ep_id IN ({placeholders}) AND state != 'ignored'"
            );
            let _ = conn.execute(&sql, rusqlite::params_from_iter(ids.iter()));
        }
        ("grabbed", Some(t)) => {
            // grabbed_title IS NULL is the discriminator: a refollow recreates
            // rows without it, while a user's deliberate "want this again"
            // (set_episode_state) preserves it — never fight the user
            let sql = format!(
                "UPDATE episodes SET state = 'grabbed', grabbed_title = ?1, grabbed_at = COALESCE(grabbed_at, ?2)
                 WHERE tvmaze_ep_id IN ({placeholders}) AND state = 'wanted' AND grabbed_title IS NULL"
            );
            let mut params: Vec<rusqlite::types::Value> =
                vec![t.to_string().into(), now().into()];
            params.extend(ids.iter().map(|i| rusqlite::types::Value::from(*i)));
            let _ = conn.execute(&sql, rusqlite::params_from_iter(params));
        }
        ("wanted", _) => {
            let sql = format!(
                "UPDATE episodes SET state = 'wanted', grabbed_title = NULL, grabbed_at = NULL, last_searched_at = 0
                 WHERE tvmaze_ep_id IN ({placeholders}) AND state = 'grabbed'"
            );
            let _ = conn.execute(&sql, rusqlite::params_from_iter(ids.iter()));
        }
        (other, _) => {
            crate::applog::info("app",format!("set_episodes_state_by_ids: unsupported state {other}"));
        }
    }
}

/// Everything already grabbed or completed for this content: (title, size).
/// The upgrade scout uses this to see what quality the user already has.
pub fn ledger_entries(conn: &Connection, content_key: &str) -> Vec<(String, i64)> {
    conn.prepare(
        "SELECT title, size FROM grab_ledger WHERE content_key = ?1 AND state IN ('grabbed','completed')",
    )
    .ok()
    .map(|mut stmt| {
        stmt.query_map([content_key], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

/// Sum of GB grabbed by one brief in the trailing 24h (rolling budget).
pub fn ledger_gb_today(conn: &Connection, brief_id: i64) -> f64 {
    conn.query_row(
        "SELECT COALESCE(SUM(size), 0) FROM grab_ledger WHERE brief_id = ?1 AND ts > ?2",
        rusqlite::params![brief_id, now() - 86_400],
        |r| r.get::<_, i64>(0),
    )
    .map(|b| b as f64 / 1e9)
    .unwrap_or(0.0)
}

pub fn ledger_grabs_today(conn: &Connection, brief_id: i64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM grab_ledger WHERE brief_id = ?1 AND ts > ?2",
        rusqlite::params![brief_id, now() - 86_400],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

// ---------- brief memory (typed K/V, capped) ----------

const MEMORY_MAX_KEYS: i64 = 40;
const MEMORY_MAX_VALUE: usize = 300;

pub fn memory_put(conn: &Connection, brief_id: i64, key: &str, value: &str) {
    let neutral = |s: &str, max: usize| -> String {
        s.chars()
            .map(|c| match c {
                '<' => '\u{2039}',
                '>' => '\u{203A}',
                c => c,
            })
            .take(max)
            .collect()
    };
    let key: String = neutral(key, 60);
    let value: String = neutral(value, MEMORY_MAX_VALUE);
    let _ = conn.execute(
        "INSERT INTO brief_memory (brief_id, key, value, ts) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(brief_id, key) DO UPDATE SET value = excluded.value, ts = excluded.ts",
        rusqlite::params![brief_id, key, value, now()],
    );
    // LRU eviction beyond the cap
    let _ = conn.execute(
        "DELETE FROM brief_memory WHERE brief_id = ?1 AND key NOT IN
           (SELECT key FROM brief_memory WHERE brief_id = ?1 ORDER BY ts DESC LIMIT ?2)",
        rusqlite::params![brief_id, MEMORY_MAX_KEYS],
    );
}

pub fn memory_digest(conn: &Connection, brief_id: i64) -> String {
    let mut stmt = match conn
        .prepare("SELECT key, value FROM brief_memory WHERE brief_id = ?1 ORDER BY ts DESC LIMIT 30")
    {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let rows: Vec<(String, String)> = stmt
        .query_map([brief_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .map(|it| it.flatten().collect())
        .unwrap_or_default();
    rows.iter()
        .map(|(k, v)| format!("- {k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------- proposals ----------

/// Upsert by content identity: re-proposing the same content refreshes the card
/// instead of stacking duplicates. Returns true when a NEW proposal was filed
/// (a refresh returns false, so callers don't re-notify on every sweep).
pub fn proposal_upsert(
    conn: &Connection,
    brief_id: Option<i64>,
    content_key: &str,
    result_json: &str,
    reason: &str,
) -> bool {
    let n = now();
    prune_dismissed_proposals(conn);
    let updated = conn
        .execute(
            // include cards mid-approval ('grabbing', fresh): they must
            // refresh in place, not fork a duplicate pending card
            "UPDATE proposals SET last_seen = ?1, reason = ?2, result_json = ?3
             WHERE content_key = ?4
               AND (status = 'pending' OR (status = 'grabbing' AND last_seen > ?5))",
            rusqlite::params![n, reason, result_json, content_key, n - 5 * 60],
        )
        .unwrap_or(0);
    if updated == 0 {
        // a stale 'grabbing' row (crash mid-approval) must be revived in
        // place, not forked into a second card for the same content
        let revived = conn
            .execute(
                "UPDATE proposals SET status = 'pending', last_seen = ?1, reason = ?2, result_json = ?3
                 WHERE content_key = ?4 AND status = 'grabbing'",
                rusqlite::params![n, reason, result_json, content_key],
            )
            .unwrap_or(0);
        if revived > 0 {
            return true;
        }
        // a recently dismissed card must not resurrect on every sweep
        let recently_dismissed = conn
            .query_row(
                "SELECT COUNT(*) FROM proposals
                 WHERE content_key = ?1 AND status = 'dismissed' AND last_seen > ?2",
                rusqlite::params![content_key, n - 7 * 86_400],
                |r| r.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .unwrap_or(false);
        if recently_dismissed {
            return false;
        }
        return conn
            .execute(
                "INSERT INTO proposals (brief_id, content_key, result_json, reason, status, first_seen, last_seen)
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?5)",
                rusqlite::params![brief_id, content_key, result_json, reason, n],
            )
            .map(|rows| rows > 0)
            .unwrap_or(false);
    }
    false
}

/// Throttle bookkeeping so unfindable episodes don't hammer indexers.
/// One transaction — a crash mid-batch must not half-stamp a season.
pub fn stamp_searched(conn: &Connection, ep_ids: &[i64]) {
    let ts = now();
    let tx = match conn.unchecked_transaction() {
        Ok(t) => t,
        Err(_) => return,
    };
    for id in ep_ids {
        let _ = tx.execute(
            "UPDATE episodes SET last_searched_at = ?1 WHERE tvmaze_ep_id = ?2",
            rusqlite::params![ts, id],
        );
    }
    let _ = tx.commit();
}

/// Stamp episodes a grab satisfied. Guarded on state = 'wanted' so a stale
/// manual grab can't flip an episode the user has since marked downloaded
/// or ignored back to grabbed — never fight the user.
pub fn mark_grabbed(conn: &Connection, ep_ids: &[i64], release_title: &str) -> Result<()> {
    let ts = now();
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    for id in ep_ids {
        tx.execute(
            "UPDATE episodes SET state = 'grabbed', grabbed_title = ?1, grabbed_at = ?2
             WHERE tvmaze_ep_id = ?3 AND state = 'wanted'",
            rusqlite::params![release_title, ts, id],
        )
        .map_err(db_err)?;
    }
    tx.commit().map_err(db_err)
}

// ---------- app meta (tiny K/V: last scout pass, migration flags) ----------

pub fn meta_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).ok()
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) {
    let _ = conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    );
}

pub fn db_err(e: rusqlite::Error) -> AppError {
    AppError::Other(format!("database error: {e}"))
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------- row types shared with the frontend ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShowRow {
    pub tvmaze_id: i64,
    pub name: String,
    pub status: String,
    pub poster_url: Option<String>,
    pub premiered: Option<String>,
    pub ended: Option<String>,
    pub network: Option<String>,
    pub imdb_id: Option<String>,
    pub followed_at: i64,
    pub refreshed_at: i64,
    pub quality_json: Option<String>,
    pub save_path_override: Option<String>,
    pub backfill: bool,
    #[serde(default)]
    pub search_alias: Option<String>,
    // aggregates for the grid
    pub total: i64,
    pub downloaded: i64,
    pub grabbed: i64,
    pub wanted: i64,
    pub next_airstamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeRow {
    pub tvmaze_ep_id: i64,
    pub show_id: i64,
    pub season: i64,
    pub number: i64,
    pub title: Option<String>,
    pub airstamp: Option<String>,
    pub state: String,
    pub grabbed_title: Option<String>,
    #[serde(default)]
    pub last_searched_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityRow {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub show_id: Option<i64>,
    pub message: String,
}

pub fn list_shows(conn: &Connection) -> Result<Vec<ShowRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT s.tvmaze_id, s.name, s.status, s.poster_url, s.premiered, s.ended,
                    s.network, s.imdb_id, s.followed_at, s.refreshed_at, s.quality_json,
                    s.save_path_override, s.backfill, s.search_alias,
                    (SELECT COUNT(*) FROM episodes e WHERE e.show_id = s.tvmaze_id AND e.state != 'ignored' AND e.state != 'upcoming') AS total,
                    (SELECT COUNT(*) FROM episodes e WHERE e.show_id = s.tvmaze_id AND e.state = 'downloaded') AS downloaded,
                    (SELECT COUNT(*) FROM episodes e WHERE e.show_id = s.tvmaze_id AND e.state = 'grabbed') AS grabbed,
                    (SELECT COUNT(*) FROM episodes e WHERE e.show_id = s.tvmaze_id AND e.state = 'wanted') AS wanted,
                    (SELECT MIN(e.airstamp) FROM episodes e WHERE e.show_id = s.tvmaze_id AND e.state = 'upcoming' AND e.airstamp IS NOT NULL) AS next_airstamp
             FROM shows s ORDER BY s.name COLLATE NOCASE",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ShowRow {
                tvmaze_id: r.get(0)?,
                name: r.get(1)?,
                status: r.get(2)?,
                poster_url: r.get(3)?,
                premiered: r.get(4)?,
                ended: r.get(5)?,
                network: r.get(6)?,
                imdb_id: r.get(7)?,
                followed_at: r.get(8)?,
                refreshed_at: r.get(9)?,
                quality_json: r.get(10)?,
                save_path_override: r.get(11)?,
                backfill: r.get::<_, i64>(12)? != 0,
                search_alias: r.get(13)?,
                total: r.get(14)?,
                downloaded: r.get(15)?,
                grabbed: r.get(16)?,
                wanted: r.get(17)?,
                next_airstamp: r.get(18)?,
            })
        })
        .map_err(db_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db_err)?;
    Ok(rows)
}

pub fn list_episodes(conn: &Connection, show_id: i64) -> Result<Vec<EpisodeRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT tvmaze_ep_id, show_id, season, number, title, airstamp, state, grabbed_title, last_searched_at
             FROM episodes WHERE show_id = ?1 ORDER BY season, number",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([show_id], |r| {
            Ok(EpisodeRow {
                tvmaze_ep_id: r.get(0)?,
                show_id: r.get(1)?,
                season: r.get(2)?,
                number: r.get(3)?,
                title: r.get(4)?,
                airstamp: r.get(5)?,
                state: r.get(6)?,
                grabbed_title: r.get(7)?,
                last_searched_at: r.get(8)?,
            })
        })
        .map_err(db_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db_err)?;
    Ok(rows)
}

pub fn log_activity(conn: &Connection, kind: &str, show_id: Option<i64>, message: &str) {
    let _ = conn.execute(
        "INSERT INTO activity (ts, kind, show_id, message) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![now(), kind, show_id, message],
    );
    // keep the log bounded
    let _ = conn.execute(
        "DELETE FROM activity WHERE id NOT IN (SELECT id FROM activity ORDER BY id DESC LIMIT 500)",
        [],
    );
}

pub fn list_activity(conn: &Connection, limit: i64) -> Result<Vec<ActivityRow>> {
    let mut stmt = conn
        .prepare("SELECT id, ts, kind, show_id, message FROM activity ORDER BY id DESC LIMIT ?1")
        .map_err(db_err)?;
    let rows = stmt
        .query_map([limit], |r| {
            Ok(ActivityRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                show_id: r.get(3)?,
                message: r.get(4)?,
            })
        })
        .map_err(db_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db_err)?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::parse_ep_ids;

    #[test]
    fn ep_ids_roundtrip_and_junk() {
        assert_eq!(parse_ep_ids(Some("[1,2,3]")), vec![1, 2, 3]);
        assert_eq!(parse_ep_ids(Some("[]")), Vec::<i64>::new());
        assert_eq!(parse_ep_ids(None), Vec::<i64>::new());
        assert_eq!(parse_ep_ids(Some("not json")), Vec::<i64>::new());
        assert_eq!(parse_ep_ids(Some("{\"a\":1}")), Vec::<i64>::new());
    }
}
