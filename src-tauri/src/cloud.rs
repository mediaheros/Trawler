//! The Bitport cloud backend at runtime: one poller that owns every call to
//! api.bitport.io, a fetcher that brings finished transfers to this machine
//! over HTTPS, and the read model the Downloads view renders.
//!
//! Lifecycle of a cloud grab (ledger states, `backend = 'bitport'`):
//!
//!   dispatching ─► grabbed ─► fetching ─► completed
//!        │            │           │
//!        │            └─► stalled ┘  (Bitport reported `error`, or the
//!        │                            files could not be listed/fetched)
//!        └─► removed / deleted      (vanished from the account / user)
//!
//! `grabbed` means Bitport is torrenting. `fetching` means every file of the
//! finished transfer has a `cloud_fetch` row and the fetcher is streaming
//! them into the grab's save path. Only a verified local copy makes an
//! episode `downloaded` — the cloud alone never does (unless the user turns
//! local fetching off in Settings).
//!
//! Every state change goes through `db::ledger_transition`, which moves a
//! row only from an expected prior state: the poller and the fetcher work
//! from snapshots taken before their awaits, and a grab the user removed in
//! the meantime must stay removed.
//!
//! Traffic discipline: the transfer listing is the whole account history
//! (140 KB on a real account, no pagination), so it is polled every
//! `ACTIVE_POLL_SECS` while something is open or the Downloads view is on
//! screen, and every `IDLE_POLL_SECS` otherwise. The view reads the cached
//! snapshot; it never calls Bitport itself.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::Manager;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, RwLock, Semaphore};

use crate::bitport::{self, BitportClient, BitportQuota, BitportTransfer, CloudFolder};
use crate::commands::normalize;
use crate::config::Config;
use crate::db::{self, CloudFetchRow, CloudLedgerRow};
use crate::error::{AppError, Result};
use crate::AppState;

/// Files downloaded at once. Two saturates a home line without turning the
/// CDN into a fan-out.
pub const FETCH_CONCURRENCY: usize = 2;
const MAX_ATTEMPTS: i64 = 6;
const ACTIVE_POLL_SECS: u64 = 15;
const IDLE_POLL_SECS: u64 = 300;
const QUOTA_REFRESH_SECS: i64 = 600;
/// how long one Downloads-view request keeps the poller on the fast cadence
const VIEW_ACTIVE_SECS: i64 = 45;
/// a finished grab stays on the Downloads page this long
const SHOW_COMPLETED_SECS: i64 = 7 * 86_400;
/// headroom demanded on the local disk beyond the file itself
const LOCAL_DISK_MARGIN: i64 = 200 * 1024 * 1024;
/// a dispatching row younger than this belongs to a grab still in flight:
/// the dispatcher has not finished writing it and the transfer may not be
/// listed yet — the poller leaves it alone entirely
const DISPATCH_GRACE_SECS: i64 = 5 * 60;
/// longest path component written to disk (bytes); leaves room for the
/// destination folder and the `.part` suffix under Windows' 260-char limit
const MAX_COMPONENT_BYTES: usize = 150;

#[derive(Debug, Clone, Default)]
pub struct CloudSnapshot {
    pub transfers: Vec<BitportTransfer>,
    /// unix seconds of the last successful listing; 0 = never
    pub fetched_at: i64,
    pub quota: Option<BitportQuota>,
    pub quota_at: i64,
    /// the last poll's failure, cleared by the next success
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct FetchProgress {
    bytes_done: i64,
    speed: f64,
    last_bytes: i64,
    last_at: Instant,
}

/// Everything the cloud backend keeps in memory. Lives in `AppState`.
pub struct CloudState {
    pub snapshot: RwLock<CloudSnapshot>,
    progress: StdMutex<HashMap<i64, FetchProgress>>,
    active_fetches: StdMutex<HashSet<i64>>,
    /// ledger ids whose in-flight downloads must stop (the grab was removed)
    cancelled: StdMutex<HashSet<i64>>,
    /// Bitport answered 401: the token is dead and Settings must say so
    pub auth_failed: AtomicBool,
    view_active_until: AtomicI64,
    /// poke the poller (after a grab, a connect, a retry)
    pub wake: Notify,
    /// poke the fetcher (new fetch rows, a retry)
    pub fetch_wake: Notify,
    /// no overall timeout — the shared client's 60 s would kill any real
    /// download; a stalled body is caught by the read timeout instead
    download_http: reqwest::Client,
}

impl Default for CloudState {
    fn default() -> Self {
        let download_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .user_agent(format!("trawler/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self {
            snapshot: RwLock::new(CloudSnapshot::default()),
            progress: StdMutex::new(HashMap::new()),
            active_fetches: StdMutex::new(HashSet::new()),
            cancelled: StdMutex::new(HashSet::new()),
            auth_failed: AtomicBool::new(false),
            view_active_until: AtomicI64::new(0),
            wake: Notify::new(),
            fetch_wake: Notify::new(),
            download_http,
        }
    }
}

impl CloudState {
    /// The Downloads view is open: poll fast for a while.
    pub fn mark_view_active(&self) {
        let until = db::now() + VIEW_ACTIVE_SECS;
        let was = self.view_active_until.swap(until, Ordering::SeqCst);
        // the first request after an idle stretch also gets a fresh listing
        if was < db::now() {
            self.wake.notify_one();
        }
    }

    fn view_active(&self) -> bool {
        self.view_active_until.load(Ordering::SeqCst) > db::now()
    }

    /// Stop every download of this grab at the next chunk. Entries are
    /// never reused: ledger ids only grow.
    pub fn cancel_fetches(&self, ledger_id: i64) {
        self.cancelled.lock().unwrap_or_else(|p| p.into_inner()).insert(ledger_id);
    }

    fn is_cancelled(&self, ledger_id: i64) -> bool {
        self.cancelled.lock().unwrap_or_else(|p| p.into_inner()).contains(&ledger_id)
    }

    fn progress_of(&self, fetch_id: i64) -> Option<(i64, f64)> {
        self.progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&fetch_id)
            .map(|p| (p.bytes_done, p.speed))
    }

    fn progress_set(&self, fetch_id: i64, bytes_done: i64) {
        let mut map = self.progress.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let entry = map.entry(fetch_id).or_insert(FetchProgress {
            bytes_done,
            speed: 0.0,
            last_bytes: bytes_done,
            last_at: now,
        });
        entry.bytes_done = bytes_done;
        let elapsed = now.duration_since(entry.last_at).as_secs_f64();
        if elapsed >= 1.0 {
            let instant = (bytes_done - entry.last_bytes) as f64 / elapsed;
            entry.speed = if entry.speed == 0.0 { instant } else { entry.speed * 0.6 + instant * 0.4 };
            entry.last_bytes = bytes_done;
            entry.last_at = now;
        }
    }

    fn progress_clear(&self, fetch_id: i64) {
        self.progress.lock().unwrap_or_else(|p| p.into_inner()).remove(&fetch_id);
    }

    fn fetch_speed_total(&self) -> f64 {
        self.progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .map(|p| p.speed)
            .sum()
    }
}

fn client<'a>(http: &'a reqwest::Client, cfg: &Config) -> BitportClient<'a> {
    BitportClient { http, token: cfg.bitport_token.clone() }
}

fn short(title: &str) -> String {
    title.chars().take(60).collect()
}

// ---------- poller ----------

pub async fn poll_loop(app: tauri::AppHandle) {
    tokio::time::sleep(Duration::from_secs(12)).await;
    loop {
        let state = app.state::<AppState>();
        let secs = match poll_once(&app, &state).await {
            Poll::NotConnected => 30,
            Poll::Active => ACTIVE_POLL_SECS,
            Poll::Idle => IDLE_POLL_SECS,
        };
        let wake = state.cloud.wake.notified();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
            _ = wake => {}
        }
    }
}

enum Poll {
    NotConnected,
    Active,
    Idle,
}

/// One listing, one reconciliation.
async fn poll_once(app: &tauri::AppHandle, state: &AppState) -> Poll {
    let cfg = state.config.read().await.clone();
    if cfg.bitport_token.is_empty() {
        *state.cloud.snapshot.write().await = CloudSnapshot::default();
        state.cloud.auth_failed.store(false, Ordering::SeqCst);
        return Poll::NotConnected;
    }
    let bp = client(&state.http, &cfg);
    let transfers = match bp.transfers().await {
        Ok(t) => t,
        Err(AppError::BitportAuth) => {
            if !state.cloud.auth_failed.swap(true, Ordering::SeqCst) {
                crate::applog::error("bitport", "Bitport rejected the access token — reconnect the account in Settings");
                let conn = state.db.lock().await;
                db::log_activity(
                    &conn,
                    "system",
                    None,
                    "Bitport no longer accepts Trawler's access — reconnect the account in Settings to resume cloud grabs",
                );
                drop(conn);
                crate::notify::dispatch(
                    app,
                    crate::notify::Kind::Error,
                    "Bitport needs reconnecting".into(),
                    "Trawler's access token was rejected. Open Settings → Connections to connect again.".into(),
                );
            }
            state.cloud.snapshot.write().await.last_error = Some("Bitport rejected the access token".into());
            return Poll::Idle;
        }
        Err(e) => {
            let mut snap = state.cloud.snapshot.write().await;
            if snap.last_error.is_none() {
                // say it once per outage, not every 15 s
                crate::applog::warn("bitport", format!("transfer poll failed: {e}"));
            }
            snap.last_error = Some(e.to_string());
            return if work_pending(state).await || state.cloud.view_active() { Poll::Active } else { Poll::Idle };
        }
    };
    state.cloud.auth_failed.store(false, Ordering::SeqCst);
    let quota_stale = {
        let mut snap = state.cloud.snapshot.write().await;
        snap.transfers = transfers.clone();
        snap.fetched_at = db::now();
        snap.last_error = None;
        db::now() - snap.quota_at >= QUOTA_REFRESH_SECS
    };
    if quota_stale {
        if let Ok(q) = bp.me().await {
            let mut snap = state.cloud.snapshot.write().await;
            snap.quota = Some(q);
            snap.quota_at = db::now();
        }
    }
    reconcile(app, state, &cfg, &bp, &transfers).await;
    finalize_all(app, state, &cfg, &bp).await;
    if work_pending(state).await || state.cloud.view_active() {
        Poll::Active
    } else {
        Poll::Idle
    }
}

/// Is anything still moving? Rows Bitport is working on, or files still to
/// bring down. A grab whose every file failed is waiting for the user and
/// does not keep the fast cadence alive.
async fn work_pending(state: &AppState) -> bool {
    let conn = state.db.lock().await;
    !db::cloud_ledger_rows(&conn, &["dispatching", "grabbed"]).is_empty() || db::cloud_fetch_open_count(&conn) > 0
}

/// Index a listing three ways. Rows carrying a token match by token ONLY —
/// an unrelated old transfer that happens to share an infohash must not
/// complete or fail somebody else's grab.
struct Listing<'a> {
    by_token: HashMap<&'a str, &'a BitportTransfer>,
    by_hash: HashMap<String, &'a BitportTransfer>,
    by_norm: HashMap<String, &'a BitportTransfer>,
}

impl<'a> Listing<'a> {
    fn new(transfers: &'a [BitportTransfer]) -> Self {
        let mut by_token = HashMap::new();
        let mut by_hash = HashMap::new();
        let mut by_norm = HashMap::new();
        for t in transfers {
            by_token.insert(t.token.as_str(), t);
            if let Some(h) = bitport::transfer_hash(t) {
                by_hash.entry(h).or_insert(t);
            }
            by_norm.entry(normalize(&t.name)).or_insert(t);
        }
        Self { by_token, by_hash, by_norm }
    }

    fn find(&self, row: &CloudLedgerRow) -> Option<&'a BitportTransfer> {
        match row.bp_token.as_deref() {
            Some(tok) => self.by_token.get(tok).copied(),
            None => row
                .info_hash
                .as_deref()
                .and_then(|h| self.by_hash.get(h).copied())
                .or_else(|| self.by_norm.get(&normalize(&row.title)).copied()),
        }
    }
}

/// Cloud-side completion and reaping, the counterpart of the qBittorrent
/// completion pass. Finished transfers become fetch plans (or completions
/// when local fetching is off), errored transfers stall their row and hand
/// the episodes back, and transfers that have VANISHED from the account
/// release their claim — but only after `AbsenceStrikes` has seen them
/// missing long enough. Rows already fetching or stalled are only watched
/// for that disappearance.
async fn reconcile(
    app: &tauri::AppHandle,
    state: &AppState,
    cfg: &Config,
    bp: &BitportClient<'_>,
    transfers: &[BitportTransfer],
) {
    let listing = Listing::new(transfers);
    let rows = {
        let conn = state.db.lock().await;
        db::cloud_ledger_rows(&conn, &["dispatching", "grabbed", "fetching", "stalled"])
    };
    let listing_empty = transfers.is_empty();
    let now = db::now();
    let grace_cutoff = now - DISPATCH_GRACE_SECS;
    static STRIKES: std::sync::OnceLock<StdMutex<crate::scheduler::AbsenceStrikes>> =
        std::sync::OnceLock::new();
    // strike bookkeeping is synchronous: the guard is !Send and must never
    // cross an await
    let verdicts: Vec<(CloudLedgerRow, Option<&BitportTransfer>, bool)> = {
        let mut strikes = STRIKES.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner());
        // said once per empty stretch, not every 15 s for half an hour
        static SAID_EMPTY: AtomicBool = AtomicBool::new(false);
        if listing_empty && rows.iter().any(|r| r.state != "stalled") {
            if !SAID_EMPTY.swap(true, Ordering::Relaxed) {
                crate::applog::warn(
                    "bitport",
                    "transfer listing came back empty while cloud grabs are open — they are handed back only if that persists for half an hour",
                );
            }
        } else if !listing_empty {
            SAID_EMPTY.store(false, Ordering::Relaxed);
        }
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            // the dispatcher owns a fresh dispatching row: it may still be
            // resolving the magnet, and confirming or binding it here would
            // make `ledger_finish_dispatch` lose its record
            if row.state == "dispatching" && row.ts >= grace_cutoff {
                strikes.present(row.id);
                continue;
            }
            let found = listing.find(&row);
            let confirmed_missing = match found {
                Some(_) => {
                    strikes.present(row.id);
                    false
                }
                None => strikes.missing(row.id, now, listing_empty) && row.ts < grace_cutoff,
            };
            out.push((row, found, confirmed_missing));
        }
        out
    };
    for (row, found, confirmed_missing) in verdicts {
        let Some(t) = found else {
            if confirmed_missing {
                vanish_row(state, &row).await;
            }
            continue;
        };
        match row.state.as_str() {
            // the fetcher and finalize own these; the transfer being listed
            // is all that matters here
            "fetching" | "stalled" => continue,
            _ => {}
        }
        {
            let conn = state.db.lock().await;
            if row.bp_token.is_none() {
                // matched by hash or name: pin the identity for every later pass
                db::ledger_set_bp_token(&conn, row.id, &t.token);
            }
            if row.state == "dispatching" {
                if let Err(error) = db::ledger_confirm_present(&conn, row.id, &row.title, &row.ep_ids) {
                    crate::applog::error("bitport", format!("could not recover pending ledger row {}: {error}", row.id));
                }
            }
        }
        if t.is_error() {
            let reason = t.message.clone().unwrap_or_else(|| "Bitport reported an error".into());
            stall_row(app, state, &row, &reason).await;
        } else if t.is_finished() {
            if cfg.bitport_fetch_to_local {
                start_fetch(app, state, &row, bp, t).await;
            } else {
                complete_row(app, state, cfg, bp, &row, &["dispatching", "grabbed"], None, Some(&t.token)).await;
            }
        }
    }
}

/// The transfer is gone from the account (deleted in Bitport's own UI, or
/// an add that never stuck). Open rows release their claim and episodes;
/// a row mid-fetch stops its downloads; a stalled row simply retires.
async fn vanish_row(state: &AppState, row: &CloudLedgerRow) {
    let conn = state.db.lock().await;
    // the row's fate first, cleanup only once it is settled: a fetch that
    // completed the row since the snapshot keeps its record and its files
    let moved = match row.state.as_str() {
        "stalled" => db::ledger_transition(&conn, row.id, &["stalled"], "removed"),
        "fetching" => db::ledger_transition(&conn, row.id, &["fetching"], "removed"),
        // episodes are handed back below, ownership-aware, not by the helper
        _ => db::ledger_confirm_missing(&conn, row.id, &[]),
    };
    match moved {
        Ok(true) => {
            if row.state == "fetching" {
                state.cloud.cancel_fetches(row.id);
                drop_parts(&db::cloud_fetch_for_ledger(&conn, row.id));
                db::cloud_fetch_delete_for_ledger(&conn, row.id);
            }
            if row.state != "stalled" {
                db::hand_back_owned_episodes(&conn, &row.ep_ids, &row.title);
            }
            if row.state != "stalled" {
                db::log_activity(
                    &conn,
                    "system",
                    None,
                    &format!("{} vanished from your Bitport cloud — Trawler can grab it again", short(&row.title)),
                );
            }
        }
        Ok(false) => {}
        Err(error) => crate::applog::error(
            "bitport",
            format!("could not retire missing ledger row {}: {error}", row.id),
        ),
    }
}

/// Remove every partial file of a grab, best effort — a download still
/// holding its file finishes the job itself once it sees the cancellation.
fn drop_parts(fetches: &[CloudFetchRow]) {
    for f in fetches {
        if f.state != "done" {
            let _ = std::fs::remove_file(part_path_for(&Path::new(&f.dest_dir).join(&f.rel_path)));
        }
    }
}

/// The cloud gave up (Bitport's own `error`, files that cannot be listed,
/// nothing fetchable). The row stalls so the planner and the dispatcher
/// refuse the same release, the episodes go back to wanted so another
/// release can be picked, and the transfer stays visible with its reason
/// until the user removes it.
async fn stall_row(app: &tauri::AppHandle, state: &AppState, row: &CloudLedgerRow, reason: &str) {
    let moved = {
        let conn = state.db.lock().await;
        // (a fetching row never stalls: its files are handled per file and
        // wait for the user's retry or remove)
        match db::ledger_transition(&conn, row.id, &["dispatching", "grabbed"], "stalled") {
            Ok(true) => {
                db::ledger_set_note(&conn, row.id, reason);
                db::hand_back_owned_episodes(&conn, &row.ep_ids, &row.title);
                // only followed episodes get re-planned; a movie or a manual
                // grab is up to the user
                let next = if row.ep_ids.is_empty() { "" } else { " — Trawler will look for another release" };
                db::log_activity(
                    &conn,
                    "system",
                    None,
                    &format!("Bitport could not deliver {}: {reason}{next}", short(&row.title)),
                );
                true
            }
            Ok(false) => false,
            Err(e) => {
                crate::applog::error("bitport", format!("could not stall ledger row {}: {e}", row.id));
                false
            }
        }
    };
    if !moved {
        return;
    }
    crate::applog::warn("bitport", format!("cloud grab failed: {} ({reason})", short(&row.title)));
    crate::notify::dispatch(
        app,
        crate::notify::Kind::Error,
        "Cloud download failed".into(),
        format!("{}\n{reason}", short(&row.title)),
    );
}

/// The transfer is finished: list its files and queue them for the fetcher.
async fn start_fetch(
    app: &tauri::AppHandle,
    state: &AppState,
    row: &CloudLedgerRow,
    bp: &BitportClient<'_>,
    t: &BitportTransfer,
) {
    let cfg = state.config.read().await.clone();
    let dest_root = resolve_dest_root(&cfg, row.save_path.as_deref());
    let plan = match plan_files(bp, t).await {
        Ok(p) => p,
        // the transfer is done; a listing that failed for any reason other
        // than a verdict about the files themselves is not the release's
        // fault — try again next poll
        Err(PlanError::Retry(e)) => {
            if !matches!(e, AppError::BitportAuth) {
                crate::applog::warn("bitport", format!("could not list the files of {} yet: {e}", short(&row.title)));
            }
            return;
        }
        Err(PlanError::Definitive(reason)) => {
            stall_row(app, state, row, &reason).await;
            return;
        }
    };
    if plan.is_empty() {
        stall_row(app, state, row, "the finished transfer holds no fetchable files").await;
        return;
    }
    let dest = dest_root.to_string_lossy().into_owned();
    let total: i64 = plan.iter().map(|p| p.size).sum();
    let count = plan.len();
    {
        let conn = state.db.lock().await;
        // idempotent against a poll that raced a previous plan
        if !db::cloud_fetch_for_ledger(&conn, row.id).is_empty() {
            let _ = db::ledger_transition(&conn, row.id, &["dispatching", "grabbed"], "fetching");
            return;
        }
        // the row must still be ours to move: a grab removed while the
        // files were being listed must not come back as fetching
        match db::ledger_transition(&conn, row.id, &["dispatching", "grabbed"], "fetching") {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                crate::applog::error("bitport", format!("could not mark ledger row {} fetching: {e}", row.id));
                return;
            }
        }
        let rows: Vec<db::NewCloudFetch<'_>> = plan
            .iter()
            .map(|p| db::NewCloudFetch {
                ledger_id: row.id,
                bp_token: &t.token,
                file_code: &p.code,
                rel_path: &p.rel_path,
                dest_dir: &dest,
                size: p.size,
            })
            .collect();
        if let Err(e) = db::cloud_fetch_insert(&conn, &rows) {
            crate::applog::error("bitport", format!("could not queue the files of {}: {e}", short(&row.title)));
            // no rows: finalize will hand the row back to grabbed for another plan
            return;
        }
        db::log_activity(
            &conn,
            "system",
            None,
            &format!(
                "Finished in the cloud: {} — bringing {count} file{} ({}) to {dest}",
                short(&row.title),
                if count == 1 { "" } else { "s" },
                fmt_bytes(total)
            ),
        );
    }
    crate::applog::info(
        "bitport",
        format!("{} finished in the cloud — fetching {count} file(s), {} to {dest}", short(&row.title), fmt_bytes(total)),
    );
    state.cloud.fetch_wake.notify_one();
}

/// A file to bring down: its cloud code, where it goes relative to the
/// destination root, and its size as the listing reported it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedFile {
    pub code: String,
    pub rel_path: String,
    pub size: i64,
}

/// Why a finished transfer could not be turned into a fetch plan.
enum PlanError {
    /// the listing itself failed (network, a rejected token, a Bitport
    /// hiccup in an envelope) — nothing is known about the files yet
    Retry(AppError),
    /// Bitport answered and the answer rules the files out
    Definitive(String),
}

/// A listing error is a verdict on the files only when Bitport says the
/// folder or file is gone; everything else (rate limits and gateway errors
/// arrive as envelopes too) deserves another try.
fn classify_listing_error(e: AppError) -> PlanError {
    let msg = e.to_string().to_ascii_lowercase();
    if msg.contains("not found") || msg.contains("not owned") || msg.contains("not your file") {
        PlanError::Definitive("its files are no longer in the cloud".into())
    } else if msg.contains("unexpected response shape") {
        // an answer that parsed but made no sense will not improve with
        // time; the user can Retry once Bitport behaves
        PlanError::Definitive("Bitport returned an unreadable file listing".into())
    } else {
        PlanError::Retry(e)
    }
}

async fn plan_files(bp: &BitportClient<'_>, t: &BitportTransfer) -> std::result::Result<Vec<PlannedFile>, PlanError> {
    if let Some(folder) = t.folder_id.as_deref() {
        let tree = bp.folder(folder, true).await.map_err(classify_listing_error)?;
        return Ok(plan_folder(&tree));
    }
    if let Some(file) = t.file_id.as_deref() {
        let f = bp.file_info(file).await.map_err(classify_listing_error)?;
        if f.virus != 0 {
            return Err(PlanError::Definitive(format!("Bitport flagged {} as unsafe (virus scan)", f.name)));
        }
        if f.size <= 0 {
            return Ok(vec![]);
        }
        return Ok(vec![PlannedFile { code: f.code, rel_path: sanitize_component(&f.name), size: f.size }]);
    }
    Err(PlanError::Definitive("the finished transfer points at neither a file nor a folder".into()))
}

/// Mirror the transfer's folder under the destination, the way qBittorrent
/// lays a multi-file torrent out. Flagged and empty files are left in the
/// cloud. Two names that sanitize to the same path (or differ only by
/// case, which Windows ignores) get a numbered suffix so neither is lost.
pub fn plan_folder(tree: &CloudFolder) -> Vec<PlannedFile> {
    fn walk(folder: &CloudFolder, base: &str, out: &mut Vec<PlannedFile>) {
        for f in &folder.files {
            if f.virus != 0 {
                crate::applog::warn("bitport", format!("skipping {} — Bitport's scanner flagged it", f.name));
                continue;
            }
            if f.size <= 0 {
                crate::applog::warn("bitport", format!("skipping {} — Bitport lists it as empty", f.name));
                continue;
            }
            out.push(PlannedFile {
                code: f.code.clone(),
                rel_path: format!("{base}/{}", sanitize_component(&f.name)),
                size: f.size,
            });
        }
        for sub in &folder.folders {
            let next = format!("{base}/{}", sanitize_component(&sub.name));
            walk(sub, &next, out);
        }
    }
    let mut out = vec![];
    walk(tree, &sanitize_component(&tree.name), &mut out);
    dedupe_paths(&mut out);
    out
}

/// Files may not share a path with each other, nor with any directory the
/// plan implies (a file `a_b` beside a folder `a_b` would block the rename
/// on every attempt).
fn dedupe_paths(files: &mut [PlannedFile]) {
    let mut seen: HashSet<String> = HashSet::new();
    for f in files.iter() {
        let mut parts: Vec<&str> = f.rel_path.split('/').collect();
        parts.pop();
        for depth in 1..=parts.len() {
            seen.insert(parts[..depth].join("/").to_lowercase());
        }
    }
    for f in files.iter_mut() {
        let mut candidate = f.rel_path.clone();
        let mut n = 1;
        while !seen.insert(candidate.to_lowercase()) {
            n += 1;
            candidate = numbered(&f.rel_path, n);
        }
        f.rel_path = candidate;
    }
}

/// `dir/name.ext` → `dir/name (n).ext`
fn numbered(rel_path: &str, n: usize) -> String {
    let (dir, name) = match rel_path.rsplit_once('/') {
        Some((d, f)) => (Some(d), f),
        None => (None, rel_path),
    };
    let renamed = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    };
    match dir {
        Some(d) => format!("{d}/{renamed}"),
        None => renamed,
    }
}

/// A cloud name as one safe path component on every platform: Windows'
/// forbidden characters and reserved device names, control characters,
/// trailing dots and spaces, the two dot entries, and a length cap that
/// keeps the extension.
pub fn sanitize_component(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    let mut out = out.trim_start().to_string();
    if out.is_empty() || out == "." || out == ".." {
        return "_".into();
    }
    if out.len() > MAX_COMPONENT_BYTES {
        out = truncate_keeping_extension(&out, MAX_COMPONENT_BYTES);
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.chars().count() == 4
            && matches!(stem.chars().nth(3), Some('1'..='9') | Some('¹') | Some('²') | Some('³')));
    if reserved {
        return format!("_{out}");
    }
    out
}

fn truncate_keeping_extension(name: &str, max_bytes: usize) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && e.len() <= 16 => (s, Some(e)),
        _ => (name, None),
    };
    let ext_len = ext.map(|e| e.len() + 1).unwrap_or(0);
    let budget = max_bytes.saturating_sub(ext_len).max(1);
    let mut cut = budget.min(stem.len());
    while cut > 0 && !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = stem[..cut].trim_end_matches([' ', '.']).to_string();
    if out.is_empty() {
        out = "_".into();
    }
    if let Some(e) = ext {
        out.push('.');
        out.push_str(e);
    }
    out
}

/// Where a grab's files land: the grab's own save path (per kind or per
/// show), else the configured cloud folder, else <Downloads>/Trawler.
pub fn resolve_dest_root(cfg: &Config, save_path: Option<&str>) -> PathBuf {
    if let Some(p) = save_path.map(str::trim).filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let configured = cfg.bitport_download_dir.trim();
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    default_download_dir()
}

pub fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Trawler")
}

/// Free bytes on the volume that holds `dir` (or its nearest existing
/// ancestor, for a folder that does not exist yet).
pub fn local_free_bytes(dir: &Path) -> Option<i64> {
    let mut probe = dir;
    loop {
        if probe.exists() {
            return fs2::available_space(probe).ok().map(|n| n.min(i64::MAX as u64) as i64);
        }
        probe = probe.parent()?;
    }
}

/// Retire a row as done, if it is still in one of `from`. `local_path` is
/// Some when the files landed here.
#[allow(clippy::too_many_arguments)]
async fn complete_row(
    app: &tauri::AppHandle,
    state: &AppState,
    cfg: &Config,
    bp: &BitportClient<'_>,
    row: &CloudLedgerRow,
    from: &[&str],
    local_path: Option<&str>,
    token: Option<&str>,
) {
    let recent = row.ts >= db::now() - 3 * 86_400; // first cycle after an upgrade may flip a backlog
    {
        let conn = state.db.lock().await;
        match db::ledger_transition(&conn, row.id, from, "completed") {
            Ok(true) => {}
            // removed meanwhile, or another pass got here first
            Ok(false) => return,
            Err(e) => {
                crate::applog::error("bitport", format!("could not complete ledger row {}: {e}", row.id));
                return;
            }
        }
        db::set_episodes_state_by_ids(&conn, &row.ep_ids, "downloaded", None);
        let message = match local_path {
            Some(p) => format!("Downloaded from the cloud: {} → {p}", short(&row.title)),
            None => format!("Finished in the cloud: {}", short(&row.title)),
        };
        db::log_activity(&conn, "complete", None, &message);
    }
    if recent {
        let (title, body) = match local_path {
            Some(p) => ("Downloaded from the cloud".to_string(), format!("{}\n{p}", short(&row.title))),
            None => ("Finished in the cloud".to_string(), short(&row.title)),
        };
        crate::notify::dispatch(app, crate::notify::Kind::Complete, title, body);
    }
    if local_path.is_some() && cfg.bitport_delete_after_fetch {
        match token {
            Some(tok) => match bp.delete_transfer(tok).await {
                Ok(()) => {
                    crate::applog::info("bitport", format!("removed {} from the cloud after a verified download", short(&row.title)));
                    let mut snap = state.cloud.snapshot.write().await;
                    snap.transfers.retain(|t| t.token != tok);
                }
                Err(e) => {
                    crate::applog::warn(
                        "bitport",
                        format!("downloaded {} but could not remove it from the cloud: {e}", short(&row.title)),
                    );
                    let conn = state.db.lock().await;
                    db::log_activity(
                        &conn,
                        "system",
                        None,
                        &format!(
                            "Downloaded {} but its cloud copy could not be removed ({e}) — delete it from the card when Bitport answers",
                            short(&row.title)
                        ),
                    );
                }
            },
            None => crate::applog::warn(
                "bitport",
                format!("downloaded {} but its cloud transfer has no known token — remove it in Bitport by hand", short(&row.title)),
            ),
        }
    }
}

/// Every `fetching` row whose files have all landed becomes completed.
/// Rows with failures wait for the user (retry / remove) — the view shows
/// the error.
async fn finalize_all(app: &tauri::AppHandle, state: &AppState, cfg: &Config, bp: &BitportClient<'_>) {
    let rows = {
        let conn = state.db.lock().await;
        db::cloud_ledger_rows(&conn, &["fetching"])
    };
    for row in rows {
        finalize_row(app, state, cfg, bp, &row).await;
    }
}

async fn finalize_row(
    app: &tauri::AppHandle,
    state: &AppState,
    cfg: &Config,
    bp: &BitportClient<'_>,
    row: &CloudLedgerRow,
) {
    let fetches = {
        let conn = state.db.lock().await;
        db::cloud_fetch_for_ledger(&conn, row.id)
    };
    if fetches.is_empty() {
        // a plan whose rows never landed (an insert that failed) — plan
        // again next poll; a row removed meanwhile is left alone by the guard
        let conn = state.db.lock().await;
        let _ = db::ledger_transition(&conn, row.id, &["fetching"], "grabbed");
        return;
    }
    if !fetches.iter().all(|f| f.state == "done") {
        return;
    }
    let local = local_root_of(&fetches);
    let token = fetches.first().map(|f| f.bp_token.clone());
    complete_row(app, state, cfg, bp, row, &["fetching"], Some(&local), token.as_deref()).await;
}

/// The one path that holds a grab locally: the transfer folder for a
/// multi-file transfer, the file itself for a single one.
fn local_root_of(fetches: &[CloudFetchRow]) -> String {
    let Some(first) = fetches.first() else { return String::new() };
    let top = first.rel_path.split('/').next().unwrap_or(&first.rel_path);
    Path::new(&first.dest_dir).join(top).to_string_lossy().into_owned()
}

// ---------- fetcher ----------

pub async fn fetch_loop(app: tauri::AppHandle) {
    tokio::time::sleep(Duration::from_secs(20)).await;
    let permits = Arc::new(Semaphore::new(FETCH_CONCURRENCY));
    loop {
        let state = app.state::<AppState>();
        let connected = !state.config.read().await.bitport_token.is_empty();
        // a dead token would just burn attempts on every file
        if connected && !state.cloud.auth_failed.load(Ordering::SeqCst) {
            let open = {
                let conn = state.db.lock().await;
                db::cloud_fetch_open(&conn)
            };
            let now = db::now();
            for row in open {
                let already = state.cloud.active_fetches.lock().unwrap_or_else(|p| p.into_inner()).contains(&row.id);
                if already || state.cloud.is_cancelled(row.ledger_id) {
                    continue;
                }
                if row.attempts > 0 && row.updated_at + backoff_secs(row.attempts) > now {
                    continue;
                }
                let Ok(permit) = permits.clone().acquire_owned().await else { break };
                state.cloud.active_fetches.lock().unwrap_or_else(|p| p.into_inner()).insert(row.id);
                let app2 = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _permit = permit;
                    fetch_task(app2, row).await;
                });
            }
        }
        let wake = state.cloud.fetch_wake.notified();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(20)) => {}
            _ = wake => {}
        }
    }
}

/// 30 s, 60 s, 2 min, 4 min, 8 min … capped at half an hour.
fn backoff_secs(attempts: i64) -> i64 {
    (30i64 << (attempts - 1).clamp(0, 6)).min(30 * 60)
}

/// Why a fetch stopped short of "done".
enum FetchFailure {
    /// the grab was removed (or its row vanished) while the download ran
    Cancelled,
    /// Bitport refused the token, or the account is disconnected — not the
    /// file's fault, so it costs no attempt
    Auth,
    /// anything else; counts against `MAX_ATTEMPTS`
    Failed(String),
}

impl From<AppError> for FetchFailure {
    fn from(e: AppError) -> Self {
        match e {
            AppError::BitportAuth => FetchFailure::Auth,
            other => FetchFailure::Failed(other.to_string()),
        }
    }
}

async fn fetch_task(app: tauri::AppHandle, row: CloudFetchRow) {
    let state = app.state::<AppState>();
    let outcome = fetch_one(&state, &row).await;
    let part = part_path_for(&Path::new(&row.dest_dir).join(&row.rel_path));
    // what a retry can resume from — visible in the bar between attempts
    let on_disk = std::fs::metadata(&part).map(|m| m.len() as i64).unwrap_or(0);
    let attempts = row.attempts + 1;
    {
        let conn = state.db.lock().await;
        match &outcome {
            Ok(()) => {}
            Err(FetchFailure::Cancelled) => {
                let _ = std::fs::remove_file(&part);
            }
            Err(FetchFailure::Auth) => {
                db::cloud_fetch_set_state(&conn, row.id, "pending", on_disk, Some("waiting for Bitport access"), false);
                // the fetcher pauses on this flag; the next listing clears
                // it if the account itself is fine — that bounds the retry
                // to the poll cadence instead of a tight loop
                state.cloud.auth_failed.store(true, Ordering::SeqCst);
                state.cloud.wake.notify_one();
            }
            Err(FetchFailure::Failed(e)) if attempts >= MAX_ATTEMPTS => {
                db::cloud_fetch_set_state(&conn, row.id, "failed", on_disk, Some(e), true);
                crate::applog::error("bitport", format!("giving up on {} after {attempts} attempts: {e}", row.rel_path));
                let file_name = row.rel_path.rsplit('/').next().unwrap_or(&row.rel_path).to_string();
                db::log_activity(
                    &conn,
                    "system",
                    None,
                    &format!("Could not bring {file_name} down from the cloud: {e}"),
                );
                // the grab is now waiting for the user: say so where they
                // listen (the cloud-side failures already do)
                let still_open = db::cloud_fetch_for_ledger(&conn, row.ledger_id)
                    .iter()
                    .any(|f| f.state == "pending" || f.state == "fetching");
                if !still_open {
                    let title = db::cloud_ledger_rows(&conn, &["fetching"])
                        .into_iter()
                        .find(|r| r.id == row.ledger_id)
                        .map(|r| short(&r.title))
                        .unwrap_or(file_name);
                    crate::notify::dispatch(
                        &app,
                        crate::notify::Kind::Error,
                        "Cloud download failed".into(),
                        format!("{title}\n{e}\nRetry it from Downloads."),
                    );
                }
            }
            Err(FetchFailure::Failed(e)) => {
                db::cloud_fetch_set_state(&conn, row.id, "pending", on_disk, Some(e), true);
                crate::applog::warn(
                    "bitport",
                    format!("fetch of {} failed (attempt {attempts}): {e} — retrying in {} s", row.rel_path, backoff_secs(attempts)),
                );
            }
        }
    }
    state.cloud.progress_clear(row.id);
    state.cloud.active_fetches.lock().unwrap_or_else(|p| p.into_inner()).remove(&row.id);
    if outcome.is_ok() {
        let cfg = state.config.read().await.clone();
        let bp = client(&state.http, &cfg);
        let ledger = {
            let conn = state.db.lock().await;
            db::cloud_ledger_rows(&conn, &["fetching"]).into_iter().find(|r| r.id == row.ledger_id)
        };
        if let Some(ledger) = ledger {
            finalize_row(&app, &state, &cfg, &bp, &ledger).await;
        }
    }
    state.cloud.fetch_wake.notify_one();
}

/// Stream one file to `<dest>/<rel_path>.part`, resuming whatever an
/// earlier attempt left behind, then verify size and CRC32 and rename.
async fn fetch_one(state: &AppState, row: &CloudFetchRow) -> std::result::Result<(), FetchFailure> {
    let cfg = state.config.read().await.clone();
    if cfg.bitport_token.is_empty() {
        return Err(FetchFailure::Auth);
    }
    if state.cloud.is_cancelled(row.ledger_id) {
        return Err(FetchFailure::Cancelled);
    }
    let bp = client(&state.http, &cfg);
    {
        let conn = state.db.lock().await;
        // zero rows updated means the grab was removed since the scan
        if !db::cloud_fetch_set_state(&conn, row.id, "fetching", row.bytes_done, None, false) {
            return Err(FetchFailure::Cancelled);
        }
    }
    // a fresh signed link every attempt: the listing's link may be a week old
    let info = bp.file_info(&row.file_code).await?;
    let size = if info.size > 0 { info.size } else { row.size };
    let crc_expected = info.crc32.clone().or_else(|| row.crc32.clone());
    {
        let conn = state.db.lock().await;
        db::cloud_fetch_set_identity(&conn, row.id, size, crc_expected.as_deref());
    }
    let url = info
        .download_url
        .ok_or_else(|| FetchFailure::Failed("Bitport returned no download link for the file".into()))?;

    let final_path = Path::new(&row.dest_dir).join(&row.rel_path);
    let part_path = part_path_for(&final_path);
    if let Some(parent) = final_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| FetchFailure::Failed(format!("could not create {}: {e}", parent.display())))?;
    }
    if let Ok(meta) = tokio::fs::metadata(&final_path).await {
        if meta.is_file() && meta.len() as i64 == size {
            // an earlier run got here — nothing to fetch
            let conn = state.db.lock().await;
            db::cloud_fetch_set_state(&conn, row.id, "done", size, None, false);
            return Ok(());
        }
    }
    let mut offset = tokio::fs::metadata(&part_path).await.map(|m| m.len() as i64).unwrap_or(0);
    if offset > size {
        let _ = tokio::fs::remove_file(&part_path).await;
        offset = 0;
    }
    if let Some(free) = local_free_bytes(Path::new(&row.dest_dir)) {
        let needed = size - offset + LOCAL_DISK_MARGIN;
        if free < needed {
            return Err(FetchFailure::Failed(format!(
                "not enough free space at {} — needs {}, has {}",
                row.dest_dir,
                fmt_bytes(needed),
                fmt_bytes(free)
            )));
        }
    }
    // resume: the checksum must cover the bytes already on disk
    let mut hasher = crc32fast::Hasher::new();
    if offset > 0 {
        let existing = part_path.clone();
        let h = tokio::task::spawn_blocking(move || -> std::io::Result<crc32fast::Hasher> {
            use std::io::Read;
            let mut hasher = crc32fast::Hasher::new();
            let mut file = std::fs::File::open(existing)?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            Ok(hasher)
        })
        .await
        .map_err(|e| FetchFailure::Failed(format!("checksum task failed: {e}")))?
        .map_err(|e| FetchFailure::Failed(format!("could not read the partial file: {e}")))?;
        hasher = h;
    }
    state.cloud.progress_set(row.id, offset);

    let mut request = state.cloud.download_http.get(&url);
    if offset > 0 && offset < size {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    if offset < size {
        let resp = request
            .send()
            .await
            .map_err(|e| FetchFailure::Failed(format!("download request failed: {}", e.without_url())))?;
        let status = resp.status();
        let append = match status.as_u16() {
            206 => true,
            200 => {
                // the CDN ignored the range: start over
                if offset > 0 {
                    hasher = crc32fast::Hasher::new();
                    offset = 0;
                }
                false
            }
            403 => return Err(FetchFailure::Failed("the download link was refused (expired or revoked)".into())),
            404 => return Err(FetchFailure::Failed("the file is no longer in the cloud".into())),
            _ => return Err(FetchFailure::Failed(format!("the download server answered {status}"))),
        };
        let mut f = if append {
            tokio::fs::OpenOptions::new().append(true).open(&part_path).await
        } else {
            tokio::fs::File::create(&part_path).await
        }
        .map_err(|e| FetchFailure::Failed(format!("could not open {}: {e}", part_path.display())))?;
        let mut resp = resp;
        loop {
            if state.cloud.is_cancelled(row.ledger_id) {
                let _ = f.flush().await;
                drop(f);
                return Err(FetchFailure::Cancelled);
            }
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    let _ = f.flush().await;
                    return Err(FetchFailure::Failed(format!(
                        "download interrupted at {}: {}",
                        fmt_bytes(offset),
                        e.without_url()
                    )));
                }
            };
            f.write_all(&chunk)
                .await
                .map_err(|e| FetchFailure::Failed(format!("could not write {}: {e}", part_path.display())))?;
            hasher.update(&chunk);
            offset += chunk.len() as i64;
            state.cloud.progress_set(row.id, offset);
            if offset > size {
                return Err(FetchFailure::Failed(format!(
                    "the download server sent more than the file's {} — refusing the file",
                    fmt_bytes(size)
                )));
            }
        }
        f.flush()
            .await
            .map_err(|e| FetchFailure::Failed(format!("could not flush {}: {e}", part_path.display())))?;
        // the handle must be gone before the rename below (Windows)
        drop(f);
    }
    if offset != size {
        return Err(FetchFailure::Failed(format!("download ended early: {} of {}", fmt_bytes(offset), fmt_bytes(size))));
    }
    if let Some(expected) = crc_expected.as_deref() {
        let got = format!("{:08x}", hasher.finalize());
        if got != expected {
            let _ = tokio::fs::remove_file(&part_path).await;
            return Err(FetchFailure::Failed(format!(
                "checksum mismatch (expected {expected}, got {got}) — the file was discarded"
            )));
        }
    }
    if state.cloud.is_cancelled(row.ledger_id) {
        return Err(FetchFailure::Cancelled);
    }
    if tokio::fs::metadata(&final_path).await.is_ok() {
        let _ = tokio::fs::remove_file(&final_path).await;
    }
    tokio::fs::rename(&part_path, &final_path)
        .await
        .map_err(|e| FetchFailure::Failed(format!("could not move the finished file into place: {e}")))?;
    {
        let conn = state.db.lock().await;
        db::cloud_fetch_set_state(&conn, row.id, "done", size, None, false);
    }
    crate::applog::info("bitport", format!("fetched {} ({})", row.rel_path, fmt_bytes(size)));
    Ok(())
}

fn part_path_for(final_path: &Path) -> PathBuf {
    let mut name = final_path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".part");
    final_path.with_file_name(name)
}

pub fn fmt_bytes(n: i64) -> String {
    let n = n.max(0) as f64;
    if n >= 1e12 {
        format!("{:.2} TB", n / 1e12)
    } else if n >= 1e9 {
        format!("{:.2} GB", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1} MB", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.0} KB", n / 1e3)
    } else {
        format!("{n:.0} B")
    }
}

// ---------- dispatch helpers ----------

/// Refuse a cloud grab that cannot succeed: an expired plan, or a cloud
/// disk without room for the release. Uses the cached quota when fresh.
pub async fn dispatch_precheck(state: &AppState, cfg: &Config, size: i64) -> Result<()> {
    let cached = {
        let snap = state.cloud.snapshot.read().await;
        snap.quota.clone().filter(|_| db::now() - snap.quota_at < QUOTA_REFRESH_SECS)
    };
    let quota = match cached {
        Some(q) => q,
        None => {
            let q = client(&state.http, cfg).me().await?;
            let mut snap = state.cloud.snapshot.write().await;
            snap.quota = Some(q.clone());
            snap.quota_at = db::now();
            q
        }
    };
    if quota.plan_expired {
        return Err(AppError::Other(
            "your Bitport plan has expired — renew it, or grab this locally with qBittorrent instead".into(),
        ));
    }
    if size > 0 && quota.disk_available < size {
        return Err(AppError::Other(format!(
            "your Bitport cloud is full — this release needs {} and the cloud has {} free",
            fmt_bytes(size),
            fmt_bytes(quota.disk_available)
        )));
    }
    Ok(())
}

/// The add response carries no identity, so look the new transfer up by its
/// btih right away (it was at the top of the listing within seconds, live).
pub async fn locate_new_transfer(bp: &BitportClient<'_>, info_hash: &str) -> Option<BitportTransfer> {
    let transfers = bp.transfers().await.ok()?;
    transfers
        .into_iter()
        .find(|t| bitport::transfer_hash(t).as_deref() == Some(&info_hash.to_ascii_lowercase()))
}

// ---------- user actions ----------

/// Give a grab another go: failed files are queued again, and a stalled
/// grab goes back to the cloud phase so the next poll re-reads the
/// transfer (a Bitport hiccup while listing the files must not be the
/// end of it). Returns how many things were retried.
pub async fn retry(state: &AppState, ledger_id: i64) -> Result<usize> {
    let n = {
        let conn = state.db.lock().await;
        let files = db::cloud_fetch_reset_failed(&conn, ledger_id);
        let stalled = db::cloud_ledger_rows(&conn, &["stalled"]).into_iter().find(|r| r.id == ledger_id);
        let row = match stalled {
            Some(row) if db::ledger_transition(&conn, ledger_id, &["stalled"], "grabbed")? => {
                db::ledger_set_note(&conn, ledger_id, "");
                // the grab is live again, so it claims its episodes again —
                // only those still wanted and unclaimed; ones a replacement
                // grab took meanwhile stay with it, and a later stall hands
                // back only what this row owns
                db::set_episodes_state_by_ids(&conn, &row.ep_ids, "grabbed", Some(&row.title));
                1
            }
            _ => 0,
        };
        files + row
    };
    state.cloud.fetch_wake.notify_one();
    state.cloud.wake.notify_one();
    Ok(n)
}

/// Delete a transfer from the cloud on the user's behalf. "Already gone" is
/// the outcome we wanted, whatever Bitport's wording — but only a listing
/// that has actually been taken can vouch for "gone"; before the first one
/// every failure is a failure. A rejected token cannot delete anything.
async fn delete_cloud_transfer(state: &AppState, cfg: &Config, token: &str) -> Result<()> {
    if cfg.bitport_token.is_empty() {
        return Err(AppError::BitportAuth);
    }
    if let Err(e) = client(&state.http, cfg).delete_transfer(token).await {
        if matches!(e, AppError::BitportAuth) {
            return Err(e);
        }
        let (listed_once, still_listed) = {
            let snap = state.cloud.snapshot.read().await;
            (snap.fetched_at > 0, snap.transfers.iter().any(|t| t.token == token))
        };
        let msg = e.to_string().to_ascii_lowercase();
        let gone_by_wording = msg.contains("not found") || msg.contains("not owned");
        if !gone_by_wording && (still_listed || !listed_once) {
            return Err(e);
        }
    }
    let mut snap = state.cloud.snapshot.write().await;
    snap.transfers.retain(|t| t.token != token);
    Ok(())
}

/// Remove a cloud grab from Trawler. For an unfinished grab: the transfer
/// (and its files) in the cloud go when asked, its downloads stop, every
/// partial file and the fetch rows go, and the episodes are handed back.
/// For a finished grab: `delete_cloud` drops the kept cloud copy and
/// nothing else (the completion record and local files stay); otherwise
/// the card is hidden and episodes stay downloaded.
pub async fn remove(state: &AppState, ledger_id: i64, delete_cloud: bool) -> Result<()> {
    let cfg = state.config.read().await.clone();
    let row = {
        let conn = state.db.lock().await;
        db::cloud_ledger_rows(&conn, &["dispatching", "grabbed", "fetching", "completed", "stalled"])
            .into_iter()
            .find(|r| r.id == ledger_id)
            .ok_or_else(|| AppError::Other("that cloud grab is no longer listed".into()))?
    };
    // Only a token the row itself learned identifies its transfer. Guessing
    // from the last listing by hash or name could pick an OLDER transfer of
    // the same release (a kept cloud copy) while the new one is still being
    // sent; a row without a token simply leaves its transfer alone — the
    // poller lists it as an orphan under "also in your cloud" later.
    let token = row.bp_token.clone();
    if row.state == "completed" {
        if delete_cloud {
            let tok = token.ok_or_else(|| AppError::Other("Trawler does not know this grab's cloud transfer any more".into()))?;
            delete_cloud_transfer(state, &cfg, &tok).await?;
            let conn = state.db.lock().await;
            db::log_activity(&conn, "system", None, &format!("Removed the cloud copy of {}", short(&row.title)));
        } else {
            let conn = state.db.lock().await;
            db::ledger_transition(&conn, ledger_id, &["completed"], "deleted")?;
        }
        state.cloud.wake.notify_one();
        return Ok(());
    }
    // the cloud delete is the fallible step, so it goes first: a grab whose
    // transfer could not be removed must keep downloading, not be left
    // half-cancelled. A dead token cannot delete anything — the user is
    // removing the grab on this side, so that proceeds, and the log says
    // what actually happened.
    let mut cloud_deleted = false;
    if delete_cloud {
        if let Some(tok) = token.as_deref() {
            match delete_cloud_transfer(state, &cfg, tok).await {
                Ok(()) => cloud_deleted = true,
                Err(AppError::BitportAuth) => {}
                Err(e) => return Err(e),
            }
        }
    }
    let conn = state.db.lock().await;
    // side effects follow the state the row is in NOW, not when we looked
    let current = db::cloud_ledger_rows(&conn, &["dispatching", "grabbed", "fetching", "completed", "stalled"])
        .into_iter()
        .find(|r| r.id == ledger_id)
        .map(|r| r.state)
        .unwrap_or_default();
    // the row's fate first; downloads and partial files only go once it is
    // settled, so a failed UPDATE cannot leave a fetching row with nothing
    // to fetch and a cancellation that never lifts
    let moved = db::ledger_transition(
        &conn,
        ledger_id,
        &["dispatching", "grabbed", "fetching", "completed", "stalled"],
        "deleted",
    )?;
    if !moved {
        state.cloud.wake.notify_one();
        return Ok(());
    }
    state.cloud.cancel_fetches(ledger_id);
    drop_parts(&db::cloud_fetch_for_ledger(&conn, ledger_id));
    db::cloud_fetch_delete_for_ledger(&conn, ledger_id);
    if current != "completed" && current != "stalled" {
        db::hand_back_owned_episodes(&conn, &row.ep_ids, &row.title);
    }
    if current != "completed" {
        let where_ = if cloud_deleted { "Removed from your cloud" } else { "Removed" };
        db::log_activity(
            &conn,
            "system",
            None,
            &format!("{where_}: {} — Trawler can grab it again", short(&row.title)),
        );
    }
    state.cloud.wake.notify_one();
    Ok(())
}

/// Disconnecting: every open cloud grab is handed back — Bitport will
/// never settle them without a token, and a download cannot get a fresh
/// link. Returns (rows that were torrenting or waiting, rows that were
/// downloading).
pub async fn release_all(state: &AppState) -> Result<(usize, usize)> {
    let conn = state.db.lock().await;
    let mut released = 0usize;
    let mut downloading = 0usize;
    for row in db::cloud_ledger_rows(&conn, &["dispatching", "grabbed", "fetching", "stalled"]) {
        if !db::ledger_transition(&conn, row.id, &["dispatching", "grabbed", "fetching", "stalled"], "removed")? {
            continue;
        }
        match row.state.as_str() {
            // a failed cloud grab has no card without a token; retiring it
            // here also lifts its block on the release for qBittorrent
            "stalled" => {}
            "fetching" => {
                state.cloud.cancel_fetches(row.id);
                drop_parts(&db::cloud_fetch_for_ledger(&conn, row.id));
                db::cloud_fetch_delete_for_ledger(&conn, row.id);
                db::hand_back_owned_episodes(&conn, &row.ep_ids, &row.title);
                downloading += 1;
            }
            _ => {
                db::hand_back_owned_episodes(&conn, &row.ep_ids, &row.title);
                released += 1;
            }
        }
    }
    Ok((released, downloading))
}

// ---------- read model for the Downloads view ----------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudItem {
    pub ledger_id: i64,
    pub title: String,
    pub token: Option<String>,
    /// sending | queued | cloud | fetching | done | error
    pub phase: String,
    /// Bitport's own word for the transfer (queued, downloading, finished…)
    pub cloud_status: String,
    pub cloud_progress: f64,
    pub message: Option<String>,
    /// the transfer still exists in the account (a done grab whose cloud
    /// copy was kept can be deleted from there)
    pub cloud_copy: bool,
    pub files_total: i64,
    pub files_done: i64,
    pub files_failed: i64,
    pub bytes_total: i64,
    pub bytes_done: i64,
    /// local download speed, bytes per second
    pub speed: f64,
    pub local_path: Option<String>,
    pub error: Option<String>,
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudView {
    pub connected: bool,
    pub auth_failed: bool,
    pub error: Option<String>,
    pub fetched_at: i64,
    pub quota: Option<BitportQuota>,
    pub items: Vec<CloudItem>,
    /// transfers in the account that no card above accounts for
    pub others: Vec<BitportTransfer>,
    pub fetch_speed: f64,
    pub fetch_to_local: bool,
}

pub async fn view(state: &AppState, cfg: &Config) -> CloudView {
    let connected = !cfg.bitport_token.is_empty();
    let snap = state.cloud.snapshot.read().await.clone();
    if !connected {
        return CloudView {
            connected: false,
            auth_failed: false,
            error: None,
            fetched_at: 0,
            quota: None,
            items: vec![],
            others: vec![],
            fetch_speed: 0.0,
            fetch_to_local: cfg.bitport_fetch_to_local,
        };
    }
    state.cloud.mark_view_active();
    let listing = Listing::new(&snap.transfers);
    let cutoff = db::now() - SHOW_COMPLETED_SECS;
    let (rows, fetches_by_ledger) = {
        let conn = state.db.lock().await;
        // open and stalled rows always (they wait for Trawler or the user);
        // finished ones for a week
        let rows: Vec<CloudLedgerRow> = db::cloud_ledger_rows(&conn, &["dispatching", "grabbed", "fetching", "completed", "stalled"])
            .into_iter()
            .filter(|r| r.state != "completed" || r.ts >= cutoff)
            .collect();
        let mut fetches: HashMap<i64, Vec<CloudFetchRow>> = HashMap::new();
        for r in &rows {
            if r.state == "fetching" || r.state == "completed" {
                fetches.insert(r.id, db::cloud_fetch_for_ledger(&conn, r.id));
            }
        }
        (rows, fetches)
    };
    let mut items: Vec<CloudItem> = rows
        .iter()
        .map(|row| build_item(state, cfg, row, listing.find(row), fetches_by_ledger.get(&row.id)))
        .collect();
    items.sort_by(|a, b| b.ts.cmp(&a.ts));
    // only transfers a card above stands for are hidden from the account
    // list — everything else stays visible and deletable, including a
    // finished grab the user hid while its cloud copy lives on
    let shown: HashSet<&str> = items.iter().filter_map(|i| i.token.as_deref()).collect();
    let others: Vec<BitportTransfer> = snap
        .transfers
        .iter()
        .filter(|t| !shown.contains(t.token.as_str()))
        .map(|t| BitportTransfer {
            // the magnet can carry private-tracker passkeys; the webview
            // has no use for it
            src: None,
            ..t.clone()
        })
        .collect();
    CloudView {
        connected,
        auth_failed: state.cloud.auth_failed.load(Ordering::SeqCst),
        error: snap.last_error.clone(),
        fetched_at: snap.fetched_at,
        quota: snap.quota.clone(),
        items,
        others,
        fetch_speed: state.cloud.fetch_speed_total(),
        fetch_to_local: cfg.bitport_fetch_to_local,
    }
}

fn build_item(
    state: &AppState,
    cfg: &Config,
    row: &CloudLedgerRow,
    transfer: Option<&BitportTransfer>,
    fetches: Option<&Vec<CloudFetchRow>>,
) -> CloudItem {
    let mut item = CloudItem {
        ledger_id: row.id,
        title: row.title.clone(),
        token: row.bp_token.clone().or_else(|| transfer.map(|t| t.token.clone())),
        phase: "cloud".into(),
        cloud_status: transfer.map(|t| t.status.clone()).unwrap_or_else(|| "waiting".into()),
        cloud_progress: transfer.map(|t| t.progress).unwrap_or(0.0),
        message: transfer.and_then(|t| t.message.clone()),
        // "there is a cloud copy Trawler can act on": the actions only ever
        // delete by the row's own token, so a hash- or name-matched transfer
        // does not count until the poller has pinned it
        cloud_copy: transfer.is_some() && row.bp_token.is_some(),
        files_total: 0,
        files_done: 0,
        files_failed: 0,
        bytes_total: row.size.max(0),
        bytes_done: 0,
        speed: 0.0,
        local_path: None,
        error: None,
        ts: row.ts,
    };
    if let Some(fs) = fetches.filter(|f| !f.is_empty()) {
        item.files_total = fs.len() as i64;
        item.files_done = fs.iter().filter(|f| f.state == "done").count() as i64;
        item.files_failed = fs.iter().filter(|f| f.state == "failed").count() as i64;
        item.bytes_total = fs.iter().map(|f| f.size).sum();
        item.bytes_done = fs
            .iter()
            .map(|f| match f.state.as_str() {
                "done" => f.size,
                _ => state.cloud.progress_of(f.id).map(|(b, _)| b).unwrap_or(f.bytes_done),
            })
            .sum();
        item.speed = fs.iter().filter_map(|f| state.cloud.progress_of(f.id)).map(|(_, s)| s).sum();
        item.local_path = Some(local_root_of(fs));
        item.error = fs.iter().find(|f| f.state == "failed").and_then(|f| f.error.clone());
        // between attempts a file sits pending with its last error and a
        // backoff of up to half an hour — silence there reads as "stuck"
        if item.speed == 0.0 && !fs.iter().any(|f| f.state == "fetching") {
            if let Some(f) = fs.iter().find(|f| f.state == "pending" && f.error.is_some()) {
                item.message = f.error.as_ref().map(|e| format!("retrying soon — {e}"));
            }
        }
    }
    item.phase = match row.state.as_str() {
        "dispatching" => "sending".into(),
        "completed" => "done".into(),
        "stalled" => {
            // Trawler's own reason (a flagged file, nothing fetchable) beats
            // the transfer's message, which is usually empty in those cases
            item.error = row
                .note
                .clone()
                .filter(|n| !n.is_empty())
                .or_else(|| item.error.clone())
                .or_else(|| item.message.clone())
                .or_else(|| Some("Bitport could not deliver this release".into()));
            "error".into()
        }
        "fetching" => {
            let open = fetches.map(|fs| fs.iter().any(|f| f.state == "pending" || f.state == "fetching")).unwrap_or(true);
            if item.files_failed > 0 && !open {
                "error".into()
            } else {
                "fetching".into()
            }
        }
        _ => match transfer {
            None => "cloud".into(),
            Some(t) if t.is_error() => {
                item.error = t.message.clone().or_else(|| Some("Bitport reported an error".into()));
                "error".into()
            }
            // finished on Bitport, not yet planned or completed here: the
            // next poll moves it on. "done" is reserved for completed rows
            // so the card's actions never outrun the record.
            Some(t) if t.is_finished() => {
                if cfg.bitport_fetch_to_local {
                    "fetching".into()
                } else {
                    "cloud".into()
                }
            }
            Some(t) if t.status == "queued" => "queued".into(),
            Some(_) => "cloud".into(),
        },
    };
    item
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitport::CloudFile;

    fn file(name: &str, code: &str, size: i64, virus: i64) -> CloudFile {
        CloudFile {
            code: code.into(),
            name: name.into(),
            size,
            kind: "video".into(),
            download_url: None,
            crc32: None,
            virus,
        }
    }

    #[test]
    fn components_are_safe_on_windows() {
        assert_eq!(sanitize_component("Show: S01E01 <x>?"), "Show_ S01E01 _x__");
        assert_eq!(sanitize_component("trailing dots... "), "trailing dots");
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(sanitize_component(""), "_");
        assert_eq!(sanitize_component("con.mkv"), "_con.mkv");
        assert_eq!(sanitize_component("COM¹"), "_COM¹");
        assert_eq!(sanitize_component("COMMON.txt"), "COMMON.txt");
        assert_eq!(sanitize_component("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_component("Sintel.mp4"), "Sintel.mp4");
        // a release name that would blow Windows' path limit keeps its extension
        let long = format!("{}.mkv", "x".repeat(400));
        let cut = sanitize_component(&long);
        assert!(cut.len() <= MAX_COMPONENT_BYTES, "{}", cut.len());
        assert!(cut.ends_with(".mkv"));
        let multibyte = format!("{}.srt", "é".repeat(300));
        let cut = sanitize_component(&multibyte);
        assert!(cut.len() <= MAX_COMPONENT_BYTES && cut.ends_with(".srt"));
    }

    #[test]
    fn folder_plans_mirror_the_tree_skip_flagged_files_and_dedupe_names() {
        let tree = CloudFolder {
            code: Some("root".into()),
            name: "Show.S01".into(),
            files: vec![
                file("Show.S01E01.mkv", "f1", 100, 0),
                file("bad.exe", "f2", 5, 1),
                file("empty.nfo", "f3", 0, 0),
                file("a?b.mkv", "f5", 10, 0),
                file("a*b.mkv", "f6", 11, 0),
                file("A_B.mkv", "f7", 12, 0),
            ],
            folders: vec![
                CloudFolder {
                    code: Some("sub".into()),
                    name: "Subs: en".into(),
                    files: vec![file("Show.S01E01.en.srt", "f4", 7, 0)],
                    folders: vec![],
                },
                // a folder named like a sibling file (after sanitizing)
                CloudFolder {
                    code: Some("clash".into()),
                    name: "a_b.mkv".into(),
                    files: vec![file("inner.txt", "f8", 3, 0)],
                    folders: vec![],
                },
            ],
        };
        let plan = plan_folder(&tree);
        assert_eq!(
            plan,
            vec![
                PlannedFile { code: "f1".into(), rel_path: "Show.S01/Show.S01E01.mkv".into(), size: 100 },
                PlannedFile { code: "f5".into(), rel_path: "Show.S01/a_b (2).mkv".into(), size: 10 },
                PlannedFile { code: "f6".into(), rel_path: "Show.S01/a_b (3).mkv".into(), size: 11 },
                PlannedFile { code: "f7".into(), rel_path: "Show.S01/A_B (4).mkv".into(), size: 12 },
                PlannedFile { code: "f4".into(), rel_path: "Show.S01/Subs_ en/Show.S01E01.en.srt".into(), size: 7 },
                PlannedFile { code: "f8".into(), rel_path: "Show.S01/a_b.mkv/inner.txt".into(), size: 3 },
            ]
        );
        assert_eq!(numbered("dir/noext", 2), "dir/noext (2)");
        assert_eq!(numbered("x.tar.gz", 3), "x.tar (3).gz");
    }

    #[test]
    fn destination_prefers_the_grab_then_the_setting_then_downloads() {
        let mut cfg = Config::default();
        assert_eq!(resolve_dest_root(&cfg, Some("D:\\TV")), PathBuf::from("D:\\TV"));
        cfg.bitport_download_dir = "E:\\Cloud".into();
        assert_eq!(resolve_dest_root(&cfg, Some("  ")), PathBuf::from("E:\\Cloud"));
        cfg.bitport_download_dir = String::new();
        assert!(resolve_dest_root(&cfg, None).ends_with("Trawler"));
    }

    #[test]
    fn part_paths_and_local_roots() {
        assert_eq!(part_path_for(Path::new("x/Show.mkv")), PathBuf::from("x/Show.mkv.part"));
        let fetches = vec![CloudFetchRow {
            id: 1,
            ledger_id: 1,
            bp_token: "t".into(),
            file_code: "f".into(),
            rel_path: "Show.S01/Subs/a.srt".into(),
            dest_dir: "D:\\TV".into(),
            size: 1,
            crc32: None,
            state: "done".into(),
            bytes_done: 1,
            attempts: 1,
            error: None,
            updated_at: 0,
        }];
        assert_eq!(local_root_of(&fetches), Path::new("D:\\TV").join("Show.S01").to_string_lossy());
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_secs(1), 30);
        assert_eq!(backoff_secs(2), 60);
        assert_eq!(backoff_secs(3), 120);
        assert_eq!(backoff_secs(9), 30 * 60);
    }

    #[test]
    fn auth_failures_cost_no_attempt() {
        assert!(matches!(FetchFailure::from(AppError::BitportAuth), FetchFailure::Auth));
        assert!(matches!(FetchFailure::from(AppError::Other("x".into())), FetchFailure::Failed(_)));
    }

    #[test]
    fn listing_matches_by_token_first_and_never_by_hash_for_tokened_rows() {
        let mk = |token: &str, name: &str, hash: &str| BitportTransfer {
            token: token.into(),
            name: name.into(),
            status: "finished".into(),
            substatus: None,
            progress: 100.0,
            size: None,
            message: None,
            file_id: None,
            folder_id: None,
            src: Some(format!("magnet:?xt=urn:btih:{hash}")),
        };
        let transfers = vec![
            mk("old", "Show.S01E01", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            mk("new", "Other", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ];
        let listing = Listing::new(&transfers);
        let row = |token: Option<&str>, hash: Option<&str>, title: &str| CloudLedgerRow {
            id: 1,
            title: title.into(),
            info_hash: hash.map(String::from),
            ep_ids: vec![],
            bp_token: token.map(String::from),
            ts: 0,
            state: "grabbed".into(),
            save_path: None,
            size: 0,
            note: None,
        };
        assert_eq!(listing.find(&row(Some("new"), None, "x")).map(|t| t.token.as_str()), Some("new"));
        assert!(
            listing.find(&row(Some("gone"), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "Show.S01E01")).is_none(),
            "a tokened row never falls back to hash or name"
        );
        assert_eq!(
            listing.find(&row(None, Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "x")).map(|t| t.token.as_str()),
            Some("old")
        );
        assert_eq!(listing.find(&row(None, None, "show s01e01")).map(|t| t.token.as_str()), Some("old"));
    }

    #[test]
    fn bytes_format_reads_well() {
        assert_eq!(fmt_bytes(999), "999 B");
        assert_eq!(fmt_bytes(1_843_217_633), "1.84 GB");
        assert_eq!(fmt_bytes(21_005), "21 KB");
    }
}
