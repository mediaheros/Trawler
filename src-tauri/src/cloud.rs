//! The Bitport cloud backend at runtime: one poller that owns every call to
//! api.bitport.io, a fetcher that brings finished transfers to this machine
//! over HTTPS, and the read model the Downloads view renders.
//!
//! Lifecycle of a cloud grab (ledger states, `backend = 'bitport'`):
//!
//!   dispatching ─► grabbed ─► fetching ─► completed
//!        │            │
//!        │            └─► stalled   (Bitport reported `error`)
//!        └─► removed / deleted      (vanished from the account / user)
//!
//! `grabbed` means Bitport is torrenting. `fetching` means every file of the
//! finished transfer has a `cloud_fetch` row and the fetcher is streaming
//! them into the grab's save path. Only a verified local copy makes an
//! episode `downloaded` — the cloud alone never does (unless the user turns
//! local fetching off in Settings).
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
    /// Bitport answered 401: the token is dead and Settings must say so
    pub auth_failed: AtomicBool,
    view_active_until: AtomicI64,
    /// poke the poller (after a grab, a connect, a retry)
    pub wake: Notify,
    /// poke the fetcher (new fetch rows, a retry)
    pub fetch_wake: Notify,
    poll_busy: AtomicBool,
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
            auth_failed: AtomicBool::new(false),
            view_active_until: AtomicI64::new(0),
            wake: Notify::new(),
            fetch_wake: Notify::new(),
            poll_busy: AtomicBool::new(false),
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

// ---------- poller ----------

pub async fn poll_loop(app: tauri::AppHandle) {
    tokio::time::sleep(Duration::from_secs(12)).await;
    loop {
        let state = app.state::<AppState>();
        let secs = if state.cloud.poll_busy.swap(true, Ordering::SeqCst) {
            ACTIVE_POLL_SECS
        } else {
            struct Busy<'a>(&'a AtomicBool);
            impl Drop for Busy<'_> {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _busy = Busy(&state.cloud.poll_busy);
            match poll_once(&app, &state).await {
                Poll::NotConnected => 30,
                Poll::Active => ACTIVE_POLL_SECS,
                Poll::Idle => IDLE_POLL_SECS,
            }
        };
        let wake = state.cloud.wake.notified();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
            _ = wake => {}
        }
    }
}

pub enum Poll {
    NotConnected,
    Active,
    Idle,
}

/// One listing, one reconciliation. Public so a connect or a grab can ask
/// for an immediate pass instead of waiting for the cadence.
pub async fn poll_once(app: &tauri::AppHandle, state: &AppState) -> Poll {
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
            return if has_open_rows(state).await || state.cloud.view_active() { Poll::Active } else { Poll::Idle };
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
    if has_open_rows(state).await || state.cloud.view_active() {
        Poll::Active
    } else {
        Poll::Idle
    }
}

async fn has_open_rows(state: &AppState) -> bool {
    let conn = state.db.lock().await;
    !db::cloud_ledger_rows(&conn, &["dispatching", "grabbed", "fetching"]).is_empty()
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
/// missing long enough.
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
        db::cloud_ledger_rows(&conn, &["dispatching", "grabbed"])
    };
    let listing_empty = transfers.is_empty();
    let now = db::now();
    let reap_cutoff = now - 5 * 60; // covers the add-to-listing gap
    static STRIKES: std::sync::OnceLock<StdMutex<crate::scheduler::AbsenceStrikes>> =
        std::sync::OnceLock::new();
    // strike bookkeeping is synchronous: the guard is !Send and must never
    // cross an await
    let verdicts: Vec<(CloudLedgerRow, Option<&BitportTransfer>, bool)> = {
        let mut strikes = STRIKES.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner());
        if listing_empty && !rows.is_empty() {
            crate::applog::warn(
                "bitport",
                "transfer listing came back empty while cloud grabs are open — claims are released only if that persists for half an hour",
            );
        }
        rows.into_iter()
            .map(|row| {
                let found = listing.find(&row);
                let confirmed_missing = match found {
                    Some(_) => {
                        strikes.present(row.id);
                        false
                    }
                    None => strikes.missing(row.id, now, listing_empty) && row.ts < reap_cutoff,
                };
                (row, found, confirmed_missing)
            })
            .collect()
    };
    for (row, found, confirmed_missing) in verdicts {
        let Some(t) = found else {
            if confirmed_missing {
                let conn = state.db.lock().await;
                match db::ledger_confirm_missing(&conn, row.id, &row.ep_ids) {
                    Ok(_) => {
                        STRIKES.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner()).present(row.id);
                        db::log_activity(
                            &conn,
                            "system",
                            None,
                            &format!(
                                "{} vanished from your Bitport cloud — its claim is released, Trawler can grab again",
                                short(&row.title)
                            ),
                        );
                    }
                    Err(error) => crate::applog::error(
                        "bitport",
                        format!("could not retire missing ledger row {}: {error}", row.id),
                    ),
                }
            }
            continue;
        };
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
            fail_row(app, state, &row, t).await;
        } else if t.is_finished() {
            if cfg.bitport_fetch_to_local {
                start_fetch(app, state, cfg, bp, &row, t).await;
            } else {
                complete_row(app, state, cfg, bp, &row, None, Some(&t.token)).await;
            }
        }
    }
}

fn short(title: &str) -> String {
    title.chars().take(60).collect()
}

/// Bitport gave up on the transfer. The row stalls (so the dispatcher
/// refuses the same release again), the episodes go back to wanted so the
/// scheduler can pick another release, and the transfer stays in the cloud
/// list with its message until the user removes it.
async fn fail_row(app: &tauri::AppHandle, state: &AppState, row: &CloudLedgerRow, t: &BitportTransfer) {
    let reason = t.message.clone().unwrap_or_else(|| "Bitport reported an error".into());
    {
        let conn = state.db.lock().await;
        if let Err(e) = db::ledger_set_state(&conn, row.id, "stalled") {
            crate::applog::error("bitport", format!("could not stall ledger row {}: {e}", row.id));
            return;
        }
        db::set_episodes_state_by_ids(&conn, &row.ep_ids, "wanted", None);
        db::log_activity(
            &conn,
            "system",
            None,
            &format!("Bitport could not download {}: {reason} — Trawler will look for another release", short(&row.title)),
        );
    }
    crate::applog::warn("bitport", format!("transfer failed in the cloud: {} ({reason})", short(&row.title)));
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
    cfg: &Config,
    bp: &BitportClient<'_>,
    row: &CloudLedgerRow,
    t: &BitportTransfer,
) {
    let dest_root = resolve_dest_root(cfg, row.save_path.as_deref());
    let plan = match plan_files(bp, t).await {
        Ok(p) => p,
        Err(e) => {
            // the transfer is done; the listing hiccup is not — try again next poll
            crate::applog::warn("bitport", format!("could not list the files of {}: {e}", short(&row.title)));
            return;
        }
    };
    if plan.is_empty() {
        crate::applog::warn("bitport", format!("{} finished in the cloud with no fetchable files", short(&row.title)));
        complete_row(app, state, cfg, bp, row, None, Some(&t.token)).await;
        return;
    }
    let dest = dest_root.to_string_lossy().into_owned();
    let total: i64 = plan.iter().map(|p| p.size).sum();
    let count = plan.len();
    {
        let conn = state.db.lock().await;
        // idempotent against a poll that raced a previous plan
        if !db::cloud_fetch_for_ledger(&conn, row.id).is_empty() {
            let _ = db::ledger_set_state(&conn, row.id, "fetching");
            return;
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
            return;
        }
        if let Err(e) = db::ledger_set_state(&conn, row.id, "fetching") {
            crate::applog::error("bitport", format!("could not mark ledger row {} fetching: {e}", row.id));
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

async fn plan_files(bp: &BitportClient<'_>, t: &BitportTransfer) -> Result<Vec<PlannedFile>> {
    if let Some(folder) = t.folder_id.as_deref() {
        let tree = bp.folder(folder, true).await?;
        return Ok(plan_folder(&tree));
    }
    if let Some(file) = t.file_id.as_deref() {
        let f = bp.file_info(file).await?;
        if f.virus != 0 {
            return Err(AppError::Other(format!("Bitport flagged {} as unsafe (virus scan)", f.name)));
        }
        return Ok(vec![PlannedFile { code: f.code, rel_path: sanitize_component(&f.name), size: f.size }]);
    }
    Err(AppError::Other("finished transfer points at neither a file nor a folder".into()))
}

/// Mirror the transfer's folder under the destination, the way qBittorrent
/// lays a multi-file torrent out. Flagged files are left in the cloud.
pub fn plan_folder(tree: &CloudFolder) -> Vec<PlannedFile> {
    fn walk(folder: &CloudFolder, base: &str, out: &mut Vec<PlannedFile>) {
        for f in &folder.files {
            if f.virus != 0 {
                crate::applog::warn("bitport", format!("skipping {} — Bitport's scanner flagged it", f.name));
                continue;
            }
            if f.size <= 0 {
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
    out
}

/// A cloud name as one safe path component on every platform: Windows'
/// forbidden characters and reserved device names, control characters,
/// trailing dots and spaces, and the two dot entries.
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
    let out = out.trim_start().to_string();
    if out.is_empty() || out == "." || out == ".." {
        return "_".into();
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
        "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        return format!("_{out}");
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

/// Retire a row as done. `local_path` is Some when the files landed here.
async fn complete_row(
    app: &tauri::AppHandle,
    state: &AppState,
    cfg: &Config,
    bp: &BitportClient<'_>,
    row: &CloudLedgerRow,
    local_path: Option<&str>,
    token: Option<&str>,
) {
    let recent = row.ts >= db::now() - 3 * 86_400; // first cycle after an upgrade may flip a backlog
    {
        let conn = state.db.lock().await;
        if let Err(e) = db::ledger_set_state(&conn, row.id, "completed") {
            crate::applog::error("bitport", format!("could not complete ledger row {}: {e}", row.id));
            return;
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
                    // the snapshot still lists it — refresh soon
                    state.cloud.wake.notify_one();
                    let mut snap = state.cloud.snapshot.write().await;
                    snap.transfers.retain(|t| t.token != tok);
                }
                Err(e) => crate::applog::warn(
                    "bitport",
                    format!("downloaded {} but could not remove it from the cloud: {e}", short(&row.title)),
                ),
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
        // planned rows lost (a manual database repair?) — plan again next poll
        let conn = state.db.lock().await;
        let _ = db::ledger_set_state(&conn, row.id, "grabbed");
        return;
    }
    if !fetches.iter().all(|f| f.state == "done") {
        return;
    }
    let local = local_root_of(&fetches);
    let token = fetches.first().map(|f| f.bp_token.clone());
    complete_row(app, state, cfg, bp, row, Some(&local), token.as_deref()).await;
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
        if connected {
            let open = {
                let conn = state.db.lock().await;
                db::cloud_fetch_open(&conn)
            };
            let now = db::now();
            for row in open {
                let already = state.cloud.active_fetches.lock().unwrap_or_else(|p| p.into_inner()).contains(&row.id);
                if already {
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

async fn fetch_task(app: tauri::AppHandle, row: CloudFetchRow) {
    let state = app.state::<AppState>();
    let outcome = fetch_one(&state, &row).await;
    let attempts = row.attempts + 1;
    {
        let conn = state.db.lock().await;
        match &outcome {
            Ok(()) => {}
            Err(e) if attempts >= MAX_ATTEMPTS => {
                db::cloud_fetch_set_state(&conn, row.id, "failed", 0, Some(&e.to_string()), false);
                crate::applog::error(
                    "bitport",
                    format!("giving up on {} after {attempts} attempts: {e}", row.rel_path),
                );
                db::log_activity(
                    &conn,
                    "system",
                    None,
                    &format!("Could not bring {} down from the cloud: {e}", row.rel_path.rsplit('/').next().unwrap_or(&row.rel_path)),
                );
            }
            Err(e) => {
                db::cloud_fetch_set_state(&conn, row.id, "pending", 0, Some(&e.to_string()), false);
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
async fn fetch_one(state: &AppState, row: &CloudFetchRow) -> Result<()> {
    let cfg = state.config.read().await.clone();
    if cfg.bitport_token.is_empty() {
        return Err(AppError::Other("Bitport is not connected".into()));
    }
    let bp = client(&state.http, &cfg);
    {
        let conn = state.db.lock().await;
        db::cloud_fetch_set_state(&conn, row.id, "fetching", row.bytes_done, None, true);
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
        .ok_or_else(|| AppError::Other("Bitport returned no download link for the file".into()))?;

    let final_path = Path::new(&row.dest_dir).join(&row.rel_path);
    let part_path = part_path_for(&final_path);
    if let Some(parent) = final_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            AppError::Other(format!("could not create {}: {e}", parent.display()))
        })?;
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
            return Err(AppError::Other(format!(
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
        .map_err(|e| AppError::Other(format!("checksum task failed: {e}")))??;
        hasher = h;
    }
    state.cloud.progress_set(row.id, offset);

    let mut request = state.cloud.download_http.get(&url);
    if offset > 0 && offset < size {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let mut file: Option<tokio::fs::File> = None;
    if offset < size {
        let resp = request.send().await.map_err(|e| AppError::Http(e.without_url()))?;
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
            403 => return Err(AppError::Other("the download link was refused (expired or revoked) — will fetch a fresh one".into())),
            404 => return Err(AppError::Other("the file is no longer in the cloud".into())),
            _ => return Err(AppError::Other(format!("the download server answered {status}"))),
        };
        let mut f = if append {
            tokio::fs::OpenOptions::new().append(true).open(&part_path).await
        } else {
            tokio::fs::File::create(&part_path).await
        }
        .map_err(|e| AppError::Other(format!("could not open {}: {e}", part_path.display())))?;
        let mut resp = resp;
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    let _ = f.flush().await;
                    return Err(AppError::Other(format!("download interrupted at {}: {}", fmt_bytes(offset), e.without_url())));
                }
            };
            f.write_all(&chunk)
                .await
                .map_err(|e| AppError::Other(format!("could not write {}: {e}", part_path.display())))?;
            hasher.update(&chunk);
            offset += chunk.len() as i64;
            state.cloud.progress_set(row.id, offset);
            if offset > size {
                return Err(AppError::Other(format!(
                    "the download server sent more than the file's {} — refusing the file",
                    fmt_bytes(size)
                )));
            }
        }
        f.flush().await.map_err(|e| AppError::Other(format!("could not flush {}: {e}", part_path.display())))?;
        file = Some(f);
    }
    drop(file);
    if offset != size {
        return Err(AppError::Other(format!("download ended early: {} of {}", fmt_bytes(offset), fmt_bytes(size))));
    }
    if let Some(expected) = crc_expected.as_deref() {
        let got = format!("{:08x}", hasher.finalize());
        if got != expected {
            let _ = tokio::fs::remove_file(&part_path).await;
            return Err(AppError::Other(format!("checksum mismatch (expected {expected}, got {got}) — the file was discarded")));
        }
    }
    if tokio::fs::metadata(&final_path).await.is_ok() {
        let _ = tokio::fs::remove_file(&final_path).await;
    }
    tokio::fs::rename(&part_path, &final_path)
        .await
        .map_err(|e| AppError::Other(format!("could not move the finished file into place: {e}")))?;
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
            "your Bitport plan has expired — renew it or switch grabs back to qBittorrent in Settings".into(),
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

/// Give a grab's failed files another go.
pub async fn retry(state: &AppState, ledger_id: i64) -> Result<usize> {
    let n = {
        let conn = state.db.lock().await;
        db::cloud_fetch_reset_failed(&conn, ledger_id)
    };
    state.cloud.fetch_wake.notify_one();
    Ok(n)
}

/// Remove a cloud grab from Trawler: the transfer (and its files) in the
/// cloud when asked, every partial file on disk, the fetch rows, and the
/// ledger claim. A finished grab keeps its local files and its episodes
/// stay downloaded; anything unfinished hands its episodes back.
pub async fn remove(state: &AppState, ledger_id: i64, delete_cloud: bool) -> Result<()> {
    let cfg = state.config.read().await.clone();
    let (row, fetches) = {
        let conn = state.db.lock().await;
        let row = db::cloud_ledger_rows(&conn, &["dispatching", "grabbed", "fetching", "completed", "stalled"])
            .into_iter()
            .find(|r| r.id == ledger_id)
            .ok_or_else(|| AppError::Other("that cloud grab is no longer listed".into()))?;
        let fetches = db::cloud_fetch_for_ledger(&conn, ledger_id);
        (row, fetches)
    };
    if delete_cloud && !cfg.bitport_token.is_empty() {
        if let Some(tok) = row.bp_token.as_deref() {
            match client(&state.http, &cfg).delete_transfer(tok).await {
                Ok(()) => {}
                // already gone is the outcome we wanted
                Err(e) if e.to_string().contains("not found") => {}
                Err(e) => return Err(e),
            }
            let mut snap = state.cloud.snapshot.write().await;
            snap.transfers.retain(|t| t.token != tok);
        }
    }
    for f in &fetches {
        if f.state != "done" {
            let part = part_path_for(&Path::new(&f.dest_dir).join(&f.rel_path));
            let _ = tokio::fs::remove_file(&part).await;
        }
    }
    let conn = state.db.lock().await;
    db::cloud_fetch_delete_for_ledger(&conn, ledger_id);
    db::ledger_set_state(&conn, ledger_id, "deleted")?;
    if row.state != "completed" {
        db::set_episodes_state_by_ids(&conn, &row.ep_ids, "wanted", None);
        db::log_activity(
            &conn,
            "system",
            None,
            &format!("Removed from your cloud: {} — released; Trawler can grab it again", short(&row.title)),
        );
    }
    state.cloud.wake.notify_one();
    Ok(())
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
    /// transfers in the account that Trawler did not create
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
    let (rows, fetches_by_ledger, all_identities) = {
        let conn = state.db.lock().await;
        let rows: Vec<CloudLedgerRow> = db::cloud_ledger_rows(&conn, &["dispatching", "grabbed", "fetching", "completed", "stalled"])
            .into_iter()
            .filter(|r| matches!(r.state.as_str(), "dispatching" | "grabbed" | "fetching") || r.ts >= cutoff)
            .collect();
        let mut fetches: HashMap<i64, Vec<CloudFetchRow>> = HashMap::new();
        for r in &rows {
            if r.state == "fetching" || r.state == "completed" {
                fetches.insert(r.id, db::cloud_fetch_for_ledger(&conn, r.id));
            }
        }
        // every transfer Trawler ever created, whatever its row's state now
        let idents: Vec<(Option<String>, Option<String>)> = conn
            .prepare("SELECT bp_token, info_hash FROM grab_ledger WHERE backend = 'bitport'")
            .ok()
            .map(|mut stmt| {
                stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.map(|h| h.to_ascii_lowercase()))))
                    .map(|it| it.flatten().collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        (rows, fetches, idents)
    };
    let mut items: Vec<CloudItem> = rows
        .iter()
        .map(|row| build_item(state, cfg, row, listing.find(row), fetches_by_ledger.get(&row.id)))
        .collect();
    items.sort_by(|a, b| b.ts.cmp(&a.ts));
    let own_tokens: HashSet<&str> = all_identities.iter().filter_map(|(t, _)| t.as_deref()).collect();
    let own_hashes: HashSet<&str> = all_identities.iter().filter_map(|(_, h)| h.as_deref()).collect();
    let others: Vec<BitportTransfer> = snap
        .transfers
        .iter()
        .filter(|t| {
            !own_tokens.contains(t.token.as_str())
                && !bitport::transfer_hash(t).map(|h| own_hashes.contains(h.as_str())).unwrap_or(false)
        })
        .cloned()
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
                _ => state.cloud.progress_of(f.id).map(|(b, _)| b).unwrap_or(0),
            })
            .sum();
        item.speed = fs.iter().filter_map(|f| state.cloud.progress_of(f.id)).map(|(_, s)| s).sum();
        item.local_path = Some(local_root_of(fs));
        item.error = fs.iter().find(|f| f.state == "failed").and_then(|f| f.error.clone());
    }
    item.phase = match row.state.as_str() {
        "dispatching" => "sending".into(),
        "completed" => "done".into(),
        "stalled" => {
            item.error = item.error.clone().or_else(|| item.message.clone()).or_else(|| Some("Bitport reported an error".into()));
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
            Some(t) if t.is_finished() => {
                if cfg.bitport_fetch_to_local {
                    "fetching".into()
                } else {
                    "done".into()
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
        assert_eq!(sanitize_component("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_component("Sintel.mp4"), "Sintel.mp4");
    }

    #[test]
    fn folder_plans_mirror_the_tree_and_skip_flagged_files() {
        let tree = CloudFolder {
            code: Some("root".into()),
            name: "Show.S01".into(),
            files: vec![file("Show.S01E01.mkv", "f1", 100, 0), file("bad.exe", "f2", 5, 1), file("empty.nfo", "f3", 0, 0)],
            folders: vec![CloudFolder {
                code: Some("sub".into()),
                name: "Subs: en".into(),
                files: vec![file("Show.S01E01.en.srt", "f4", 7, 0)],
                folders: vec![],
            }],
        };
        let plan = plan_folder(&tree);
        assert_eq!(
            plan,
            vec![
                PlannedFile { code: "f1".into(), rel_path: "Show.S01/Show.S01E01.mkv".into(), size: 100 },
                PlannedFile { code: "f4".into(), rel_path: "Show.S01/Subs_ en/Show.S01E01.en.srt".into(), size: 7 },
            ]
        );
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
