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

/// Case-insensitive ASCII substring search on raw bytes. Needles are ASCII,
/// and ASCII bytes never occur inside a multi-byte UTF-8 sequence, so byte
/// offsets from this are always char boundaries in the haystack. (The naive
/// to_lowercase() version desynced offsets on Turkish İ — leaking secret
/// prefixes or aborting the process. Reviewer receipts in the tests.)
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    h.windows(n.len()).position(|w| w.eq_ignore_ascii_case(n))
}

/// Where a masked value ends. Query-style (=) values stop at whitespace or
/// structure; header-style (:) values run to end of line — headers contain
/// spaces ("Bearer abc"), and under-masking is the one unacceptable direction.
fn value_end(tail: &str, header_style: bool) -> usize {
    if header_style {
        tail.find(['\n', '\r', '"', ',', '}'])
            .unwrap_or(tail.len())
    } else {
        tail.find(|c: char| {
            c.is_whitespace() || matches!(c, '&' | '"' | '\'' | ',' | '}' | ')' | ';' | '<' | '>')
        })
        .unwrap_or(tail.len())
    }
}

/// Strip anything credential-shaped before it can enter the buffer:
/// key=value / key: value pairs, URL path segments that look like passkeys,
/// and any long bare hex run. Public-paste safety is the contract here.
pub(crate) fn scrub(msg: &str) -> String {
    // pass 1: known credential keys followed by = : or %3D
    const KEYS: &[&str] = &[
        "apikey", "api_key", "api-key", "passkey", "torrent_pass", "rsskey", "password", "secret",
        "token", "authkey", "authorization", "bearer", "x-api-key",
    ];
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    loop {
        let hit = KEYS
            .iter()
            .filter_map(|k| find_ci(rest, k).map(|i| (i, k.len())))
            .min_by_key(|(i, _)| *i);
        let Some((i, klen)) = hit else {
            out.push_str(rest);
            break;
        };
        let after_key = i + klen;
        let tail = &rest[after_key..];
        // accept "=", ":", "%3D" (any case), with optional following space
        let sep_len = if tail.starts_with('=') || tail.starts_with(':') {
            1
        } else if tail.starts_with("\":") {
            // JSON: "apikey":"value"
            2
        } else if tail
            .as_bytes()
            .get(..3)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"%3d"))
        {
            // byte-level: `tail[..3]` panics when a multibyte char straddles
            // the cut ("token😀"), and a panic here aborts the release build
            3
        } else {
            // the word appeared without a separator ("bearer abc"): treat one
            // space as the separator for authorization/bearer only
            if tail.starts_with(' ') && (rest[i..after_key].eq_ignore_ascii_case("bearer") || rest[i..after_key].eq_ignore_ascii_case("authorization")) {
                1
            } else {
                out.push_str(&rest[..after_key]);
                rest = tail;
                continue;
            }
        };
        let header_style = tail.starts_with(':');
        out.push_str(&rest[..after_key + sep_len]);
        out.push_str("•••");
        let mut vstart = sep_len;
        while tail[vstart..].starts_with(' ') {
            vstart += 1;
        }
        if tail[vstart..].starts_with('"') {
            vstart += 1; // opening quote of a JSON value — the mask goes inside
        }
        let val = &tail[vstart..];
        rest = &val[value_end(val, header_style)..];
    }

    // pass 1.5: known credential-bearing hosts whose first path segment IS the
    // secret (Bitport's HTTPS directory key is 16 alphanumerics — not hex, so
    // the generic pass below can't see it)
    let out = {
        let mut o = String::with_capacity(out.len());
        let mut rest = out.as_str();
        const HOST: &str = "dir.bitport.io/";
        while let Some(i) = rest.find(HOST) {
            let end = i + HOST.len();
            o.push_str(&rest[..end]);
            o.push_str("•••");
            let tail = &rest[end..];
            let stop = tail
                .find(|c: char| c == '/' || c.is_whitespace() || c == '"' || c == '\'')
                .unwrap_or(tail.len());
            rest = &tail[stop..];
        }
        o.push_str(rest);
        o
    };

    // pass 2: hex runs (a tracker passkey as a PATH segment of >=16 hex, or
    // any bare >=20-char hex run) and mixed letter+digit path segments of
    // >=20 chars ending at a URL delimiter (base32 / alphanumeric passkeys).
    // Folder names are letters only or carry dots and hyphens, so the
    // support bundle keeps them readable.
    let bytes: Vec<char> = out.chars().collect();
    let mut result = String::with_capacity(out.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if !c.is_ascii_alphanumeric() {
            result.push(c);
            i += 1;
            continue;
        }
        let start = i;
        let after_slash = start > 0 && bytes[start - 1] == '/';
        let mut hex_end = start;
        while hex_end < bytes.len() && (bytes[hex_end].is_ascii_hexdigit() || bytes[hex_end] == '-') {
            hex_end += 1;
        }
        let hexlen = bytes[start..hex_end].iter().filter(|c| c.is_ascii_hexdigit()).count();
        let hex_in_path = after_slash && hex_end < bytes.len() && bytes[hex_end] == '/';
        if hexlen > 0 && ((hex_in_path && hexlen >= 16) || hexlen >= 20) {
            result.push_str("•••");
            i = hex_end;
            continue;
        }
        if after_slash {
            let mut seg_end = start;
            while seg_end < bytes.len() && bytes[seg_end].is_ascii_alphanumeric() {
                seg_end += 1;
            }
            let terminated = seg_end == bytes.len()
                || bytes[seg_end].is_whitespace()
                || matches!(bytes[seg_end], '/' | '?' | '&' | '#' | '"' | '\'' | ')');
            let segment = &bytes[start..seg_end];
            let mixed = segment.iter().any(|c| c.is_ascii_digit())
                && segment.iter().any(|c| c.is_ascii_alphabetic());
            if terminated && segment.len() >= 20 && mixed {
                result.push_str("•••");
                i = seg_end;
                continue;
            }
        }
        if hex_end > start {
            result.extend(bytes[start..hex_end].iter());
            i = hex_end;
        } else {
            result.push(c);
            i += 1;
        }
    }
    result
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
        assert_eq!(scrub("GET /api?q=ufc&apikey=deadbeef123&limit=5"), "GET /api?q=ufc&apikey=•••&limit=5");
        assert_eq!(scrub("passkey=abc123 done"), "passkey=••• done");
        assert_eq!(scrub("nothing secret here"), "nothing secret here");
        assert_eq!(scrub("a=1&apikey=x&b=2&token=y"), "a=1&apikey=•••&b=2&token=•••");
    }

    #[test]
    fn scrub_survives_unicode_and_leaks_nothing() {
        // Turkish İ changes byte length under to_lowercase — the old version
        // panicked or leaked the secret's head on exactly these
        // a credential word followed by a multibyte char: the "%3D" probe
        // must not byte-slice into the middle of it (panic = abort in release)
        assert_eq!(scrub("token😀"), "token😀");
        assert_eq!(scrub("search \"passwordéé\" -> 0 releases"), "search \"passwordéé\" -> 0 releases");
        assert_eq!(scrub("İİ apikey=x"), "İİ apikey=•••");
        assert_eq!(scrub("İ apikey=SECRET&x=1"), "İ apikey=•••&x=1");
        assert_eq!(scrub("İ apikey=Ünicode"), "İ apikey=•••");
        assert_eq!(scrub("Istanbullu Gelin İ apikey=S3cr3t&x=1"), "Istanbullu Gelin İ apikey=•••&x=1");
    }

    #[test]
    fn scrub_covers_header_json_path_shapes() {
        assert_eq!(scrub("X-Api-Key: abc123"), "X-Api-Key:•••");
        assert_eq!(scrub("Authorization: Bearer abcdef"), "Authorization:•••");
        assert!(scrub("{\"apikey\":\"abc123\"}").contains("apikey"));
        assert!(!scrub("{\"apikey\":\"abc123\"}").contains("abc123"));
        // tracker passkey as a PATH segment
        assert_eq!(
            scrub("https://tr.example/abc123deadbeef00/announce failed"),
            "https://tr.example/•••/announce failed"
        );
        // long bare hex (infohash-adjacent secrets)
        assert!(!scrub("blob deadbeefdeadbeefdeadbeef11 end").contains("deadbeefdeadbeefdeadbeef11"));
        // newline must terminate a masked value
        let multi = scrub("password=hunter2\nnext line kept?");
        assert!(multi.contains("next line kept?"));
        assert!(!multi.contains("hunter2"));
        // short hex stays (episode hashes in titles etc. under 16 chars)
        assert_eq!(scrub("group CAFE12 fine"), "group CAFE12 fine");
        // Bitport HTTPS-directory keys are the whole secret
        assert_eq!(
            scrub("fetch https://dir.bitport.io/qszik7hip9xqfj26/My%20Files/x.mkv done"),
            "fetch https://dir.bitport.io/•••/My%20Files/x.mkv done"
        );
    }

    #[test]
    fn scrub_covers_tracker_credential_shapes() {
        // private trackers: torrent_pass / rsskey query keys, and base32 or
        // mixed-alphanumeric passkeys as a path segment (not only hex)
        assert_eq!(scrub("announce?torrent_pass=abc123xyz&x=1"), "announce?torrent_pass=•••&x=1");
        assert_eq!(scrub("rss?rsskey=deadbeef99 ok"), "rss?rsskey=••• ok");
        assert_eq!(scrub("secret=hunter2&"), "secret=•••&");
        assert_eq!(
            scrub("https://tr.example/MFRGGZDFMZTWQ2LKNNWGC3TFON2A/announce timed out"),
            "https://tr.example/•••/announce timed out"
        );
        // release names in a path keep their dots and hyphens and stay readable
        assert_eq!(
            scrub("/downloads/Show.S01E01.1080p.WEB-DL.x265-GROUP/file.mkv"),
            "/downloads/Show.S01E01.1080p.WEB-DL.x265-GROUP/file.mkv"
        );
        // a passkey as the LAST path segment, or right before the query
        assert_eq!(
            scrub("https://tracker/rss/MFRGGZDFMZTWQ2LKNNWGC3TFON2A?cat=5"),
            "https://tracker/rss/•••?cat=5"
        );
        assert_eq!(scrub("GET /download/12345/a1B2c3D4e5F6g7H8i9J0k1"), "GET /download/12345/•••");
        // a long hex run followed by another letter is still a hex secret
        assert_eq!(
            scrub("/x/0123456789abcdef0123456789abcdef01234567z"),
            "/x/•••z"
        );
        // long folder names made of letters only are paths, not keys - the
        // support bundle needs them readable
        assert_eq!(
            scrub("/mnt/media/TelevisionSeriesArchive/Show/file.mkv"),
            "/mnt/media/TelevisionSeriesArchive/Show/file.mkv"
        );
    }
}
