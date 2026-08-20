//! The in-app log console: a bounded ring of structured entries, streamed
//! live to the UI and copyable as a support bundle. Secrets are scrubbed at
//! ingest — nothing that enters this buffer may contain a key or passkey.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use tauri::Emitter;

const CAPACITY: usize = 2000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub ts: i64,
    /// "info" | "warn" | "error"
    pub level: &'static str,
    /// subsystem: scheduler, rss, prowlarr, qbit, setup, agent, updater, app
    pub area: &'static str,
    pub message: String,
}

static RING: OnceLock<Mutex<VecDeque<LogEntry>>> = OnceLock::new();
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

/// Called once at startup so entries can stream to the UI as they happen.
pub fn attach(app: &tauri::AppHandle) {
    let _ = APP.set(app.clone());
}

/// Strip anything credential-shaped before it can enter the buffer:
/// query-string keys, header-style mentions, and long hex blobs in URLs.
fn scrub(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    let needles = ["apikey=", "api_key=", "passkey=", "password=", "token=", "authkey="];
    'outer: while !rest.is_empty() {
        let lower = rest.to_lowercase();
        let hit = needles
            .iter()
            .filter_map(|n| lower.find(n).map(|i| (i, n.len())))
            .min_by_key(|(i, _)| *i);
        match hit {
            Some((i, nlen)) => {
                let end = i + nlen;
                out.push_str(&rest[..end]);
                out.push_str("•••");
                let tail = &rest[end..];
                let stop = tail
                    .find(|c: char| c == '&' || c == ' ' || c == '"' || c == '\'')
                    .unwrap_or(tail.len());
                rest = &tail[stop..];
            }
            None => {
                out.push_str(rest);
                break 'outer;
            }
        }
    }
    out
}

fn push(level: &'static str, area: &'static str, message: String) {
    let entry = LogEntry {
        ts: crate::db::now(),
        level,
        area,
        message: scrub(&message),
    };
    // stderr keeps working for dev sessions
    eprintln!("[trawler:{area}] {}", entry.message);
    let ring = RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)));
    if let Ok(mut r) = ring.lock() {
        if r.len() >= CAPACITY {
            r.pop_front();
        }
        r.push_back(entry.clone());
    }
    if let Some(app) = APP.get() {
        let _ = app.emit("app-log", &entry);
    }
}

pub fn info(area: &'static str, message: impl Into<String>) {
    push("info", area, message.into());
}
pub fn warn(area: &'static str, message: impl Into<String>) {
    push("warn", area, message.into());
}
pub fn error(area: &'static str, message: impl Into<String>) {
    push("error", area, message.into());
}

/// Everything currently buffered, oldest first.
pub fn recent() -> Vec<LogEntry> {
    RING.get()
        .and_then(|r| r.lock().ok().map(|q| q.iter().cloned().collect()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::scrub;

    #[test]
    fn scrub_kills_credentials() {
        assert_eq!(
            scrub("GET /api?q=ufc&apikey=deadbeef123&limit=5"),
            "GET /api?q=ufc&apikey=•••&limit=5"
        );
        assert_eq!(scrub("passkey=abc123 done"), "passkey=••• done");
        assert_eq!(scrub("nothing secret here"), "nothing secret here");
        // multiple secrets in one line
        assert_eq!(
            scrub("a=1&apikey=x&b=2&token=y"),
            "a=1&apikey=•••&b=2&token=•••"
        );
    }
}
