//! Claim-then-dispatch grab machinery.
//!
//! Every path that sends a torrent to qBittorrent goes through `dispatch`:
//! - the content key is *claimed* first, so concurrent paths (scheduler
//!   cycle, RSS sweep, agent runs, proposal approvals, manual grabs) can't
//!   double-grab the same release;
//! - the qBittorrent add AND the ledger recording run on a task of their
//!   own, so a caller dropped at a run deadline (tokio timeout) can't
//!   orphan a torrent qBittorrent already accepted — without the ledger
//!   row, every later pass sees the content as missing and grabs it again.

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};

use crate::commands::perform_grab_core;
use crate::db;
use crate::error::{AppError, Result};
use crate::AppState;

fn uses_bitport(cfg: &crate::config::Config) -> bool {
    cfg.download_backend == "bitport" && !cfg.bitport_token.is_empty()
}

/// The free-space floor every automatic grab path checks against, in bytes.
/// Never below 5 GB whatever Settings says: the scheduler, the RSS sweep and
/// the agent used to disagree on this, and a catalog import must not be able
/// to fill a disk because one path trusted a 0 GB setting.
pub fn min_free_bytes(cfg: &crate::config::Config) -> f64 {
    cfg.agent_min_free_disk_gb.max(5.0) * 1e9
}

async fn selected_backend_free_bytes_with<BitportProbe, BitportFuture, QbitProbe, QbitFuture>(
    cfg: &crate::config::Config,
    bitport_probe: BitportProbe,
    qbit_probe: QbitProbe,
) -> Result<i64>
where
    BitportProbe: FnOnce() -> BitportFuture,
    BitportFuture: Future<Output = Result<i64>>,
    QbitProbe: FnOnce() -> QbitFuture,
    QbitFuture: Future<Output = Result<i64>>,
{
    if uses_bitport(cfg) {
        bitport_probe().await
    } else {
        qbit_probe().await
    }
}

/// Available bytes on the backend that will actually receive the grab.
/// Cloud grabs must not be blocked by an unrelated local qBittorrent disk,
/// and local grabs must never be authorized from Bitport's cloud quota.
pub async fn selected_backend_free_bytes(
    http: &reqwest::Client,
    cfg: &crate::config::Config,
) -> Result<i64> {
    selected_backend_free_bytes_with(
        cfg,
        || async {
            let client = crate::bitport::BitportClient {
                http,
                token: cfg.bitport_token.clone(),
            };
            Ok(client.me().await?.disk_available)
        },
        || async {
            let client = crate::qbit::QbitClient {
                http,
                base: cfg.qbit_url.clone(),
                username: cfg.qbit_username.clone(),
                password: cfg.qbit_password.clone(),
            };
            client.free_space().await
        },
    )
    .await
}

/// In-flight grab claims, keyed by content key. The lock is a std Mutex and
/// its critical section is a hash insert/remove — it must never span an
/// await.
#[derive(Default)]
pub struct GrabClaims {
    inner: Mutex<HashSet<String>>,
}

/// Releases the claim when dropped — including on panic and on caller
/// cancellation (the guard is owned by the dispatch task, which runs to
/// completion even then).
pub struct ClaimGuard {
    claims: Arc<GrabClaims>,
    key: String,
}

impl GrabClaims {
    /// Claim `key`; `None` when another path holds it.
    pub fn claim(self: &Arc<Self>, key: &str) -> Option<ClaimGuard> {
        let mut set = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if set.insert(key.to_string()) {
            Some(ClaimGuard { claims: Arc::clone(self), key: key.to_string() })
        } else {
            None
        }
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        let mut set = self.claims.inner.lock().unwrap_or_else(|p| p.into_inner());
        set.remove(&self.key);
    }
}

/// One grab request, provenance included for the ledger row.
pub struct GrabOrder {
    pub title: String,
    pub magnet_url: Option<String>,
    pub download_url: Option<String>,
    pub save_path: Option<String>,
    pub info_hash: Option<String>,
    pub size: i64,
}

pub enum GrabOutcome {
    /// Accepted by the chosen backend; ledger row written, episodes stamped.
    Grabbed { backend: &'static str },
    /// Another path is grabbing this content right now — treat as handled.
    AlreadyClaimed,
    /// The ledger already counts this content — nothing to do.
    AlreadyHad,
}

/// Only a real magnet link travels as one. Indexer definitions are free-form
/// and can put an http(s) link (passkey included) in the magnet field; that
/// must be fetched through Prowlarr like any download link, never POSTed to
/// Bitport or handed to qBittorrent as a URL to fetch itself.
pub fn split_sources(
    magnet_url: Option<String>,
    download_url: Option<String>,
) -> (Option<String>, Option<String>) {
    let magnet_url = magnet_url.filter(|m| !m.trim().is_empty());
    let download_url = download_url.filter(|d| !d.trim().is_empty());
    match magnet_url {
        Some(m) if m.trim_start().get(..7).is_some_and(|p| p.eq_ignore_ascii_case("magnet:")) => {
            (Some(m), download_url)
        }
        Some(link) => (None, download_url.or(Some(link))),
        None => (None, download_url),
    }
}

pub async fn dispatch(
    state: &AppState,
    order: GrabOrder,
    brief_id: Option<i64>,
    ep_ids: Vec<i64>,
) -> Result<GrabOutcome> {
    let mut order = order;
    let (magnet_url, download_url) = split_sources(order.magnet_url.take(), order.download_url.take());
    order.magnet_url = magnet_url;
    order.download_url = download_url;
    // Persist the strongest backend identity before the durable claim. A
    // magnet torrent can be renamed as soon as qBittorrent receives metadata;
    // after a crash, title-only reconciliation could otherwise miss a transfer
    // the backend accepted and release the claim for a duplicate grab.
    if order.info_hash.is_none() {
        order.info_hash = crate::scheduler::magnet_hash(order.magnet_url.as_deref());
    }
    let ck = crate::briefs::content_key(&order.title);
    let Some(claim) = state.grab_claims.claim(&ck) else {
        return Ok(GrabOutcome::AlreadyClaimed);
    };
    let http = state.http.clone();
    let cfg = state.config.read().await.clone();
    let title = order.title.clone();
    // the ADDITIVE cloud backend: same claim, same ledger, same episode
    // linkage — only the transport differs. Chosen per config, never forced.
    let use_bitport = uses_bitport(&cfg);
    let backend: &'static str = if use_bitport { "bitport" } else { "qbittorrent" };
    {
        let conn = state.db.lock().await;
        if !db::ledger_claim_dispatch(&conn, &db::DispatchClaim {
            content_key: &ck,
            brief_id,
            title: &order.title,
            info_hash: order.info_hash.as_deref(),
            size: order.size,
            ep_ids: &ep_ids,
            backend,
        })? {
            return Ok(GrabOutcome::AlreadyHad);
        }
    }
    // moved into the task: a definitive backend failure hands these back
    let handle = tauri::async_runtime::spawn(async move {
        let _claim = claim; // released when the grab settles, even on panic
        let dispatch_result: Result<Option<String>> = async {
            if use_bitport {
            // Bitport receives a magnet and nothing else: a download_url can
            // carry Prowlarr's API key or a private-tracker passkey, and
            // .torrent bytes embed the passkey in their announce URL — none
            // of that may leave this machine. Resolve through local Prowlarr;
            // most "torrent" links are magnet redirects anyway.
            let src = if let Some(m) = order.magnet_url.clone() {
                m
            } else if let Some(du) = order.download_url.clone() {
                let p = crate::commands::prowlarr_pub(&http, &cfg)?;
                let (_bytes, magnet) = p.fetch_torrent(&du).await?;
                match magnet {
                    Some(m) => m,
                    None => {
                        return Err(crate::error::AppError::Other(
                            "this release only offers a .torrent file — cloud grabs need a magnet link (uploading the file would hand tracker credentials to Bitport)".into(),
                        ))
                    }
                }
            } else {
                return Err(crate::error::AppError::Other(
                    "this release offers no magnet or download link".into(),
                ))
            };
            // the magnet's btih makes cloud completion/reaper matching exact
            // even when the search result carried no info_hash
            if order.info_hash.is_none() {
                order.info_hash = crate::scheduler::magnet_hash(Some(&src));
                if let Some(info_hash) = order.info_hash.as_deref() {
                    let conn = db::open_existing()?;
                    db::ledger_set_dispatch_info_hash(&conn, &ck, info_hash)?;
                }
            }
            let bp = crate::bitport::BitportClient { http: &http, token: cfg.bitport_token.clone() };
            let tok = bp.add_transfer(&src).await?;
            crate::applog::info("bitport", format!("sent to cloud: {}", order.title.chars().take(70).collect::<String>()));
                Ok(tok)
            } else {
                let core = perform_grab_core(&http, &cfg, &order, &ck).await?;
                // the hash qBittorrent tracks wins over the search result's
                // claim, and it is what ledger_finish_dispatch records below —
                // otherwise a stale indexer hash is written straight back
                // over the corrected one
                if core.info_hash.is_some() {
                    order.info_hash = core.info_hash;
                }
                if core.duplicate {
                    // "Fails." means either the torrent is already in
                    // qBittorrent (a reap that fired on a partial listing, or
                    // the user added it by hand) or qBittorrent could not
                    // parse it. Only the first is a grab: confirm by hash.
                    // Abandoning a real duplicate made the scheduler pick the
                    // same top release again every cycle, forever.
                    let q = crate::commands::qbit(&http, &cfg);
                    // a listing failure here is not a verdict: the add said
                    // "already have it", so keep the pending row for the
                    // completion pass to reconcile instead of abandoning it
                    let torrents = q.list(None).await.map_err(|e| {
                        AppError::DispatchUncertain(format!(
                            "qBittorrent reported the torrent as already present but the listing failed ({e}); Trawler will reconcile it"
                        ))
                    })?;
                    let wanted_norm = crate::commands::normalize(&order.title);
                    // by hash first; then by normalized name, the way the
                    // reaper matches. The name fallback also covers a hash
                    // the indexer got wrong: qBittorrent's own hash is the
                    // one that counts and is recorded below.
                    let mut torrents = torrents;
                    let idx = order
                        .info_hash
                        .as_deref()
                        .and_then(|h| torrents.iter().position(|t| t.hash.eq_ignore_ascii_case(h)))
                        .or_else(|| {
                            torrents
                                .iter()
                                .position(|t| crate::commands::normalize(&t.name) == wanted_norm)
                        });
                    let Some(existing) = idx.map(|i| torrents.swap_remove(i)) else {
                        return Err(AppError::Other(format!(
                            "qBittorrent rejected \"{}\" — it could not parse the torrent",
                            order.title.chars().take(70).collect::<String>()
                        )));
                    };
                    // The dead-swarm medic's corpse is known from the ledger,
                    // not from qBittorrent's state: a torrent may be stopped
                    // for many benign reasons (added paused, finished under a
                    // stop-when-complete seed policy, paused by hand) and all
                    // of those are adoptable. A stalled ledger row for THIS
                    // torrent means Trawler already judged it dead; parking
                    // the episodes on it again would leave them there forever.
                    // A stalled row that carries a different comparable hash
                    // is a different torrent (the same release re-uploaded on
                    // another tracker) and must not block a healthy copy.
                    let is_corpse = {
                        // the torrent is proven present; a database hiccup
                        // here is not grounds to abandon the claim
                        let conn = db::open_existing().map_err(|e| {
                            AppError::DispatchUncertain(format!(
                                "qBittorrent has the torrent but the ledger could not be opened ({e}); Trawler will reconcile it"
                            ))
                        })?;
                        let norm = crate::commands::normalize(&existing.name);
                        conn.prepare(
                            "SELECT title, info_hash FROM grab_ledger WHERE state = 'stalled' AND backend = 'qbittorrent'",
                        )
                        .ok()
                        .and_then(|mut stmt| {
                            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
                                .ok()
                                .map(|rows| {
                                    rows.flatten().any(|(title, hash)| match hash {
                                        Some(h) if crate::scheduler::qbt_comparable_hash(&h) => {
                                            h.eq_ignore_ascii_case(&existing.hash)
                                        }
                                        _ => {
                                            let t = crate::commands::normalize(&title);
                                            t == norm || t == wanted_norm
                                        }
                                    })
                                })
                        })
                        .unwrap_or(false)
                    };
                    if is_corpse {
                        return Err(AppError::Other(format!(
                            "qBittorrent already has \"{}\" but Trawler paused it as a dead swarm — pick another release, or resume it by hand",
                            order.title.chars().take(70).collect::<String>()
                        )));
                    }
                    // a torrent whose payload is gone or that qBittorrent has
                    // flagged as broken must not become a "downloaded" episode
                    if existing.state == "missingFiles" || existing.state == "error" {
                        return Err(AppError::Other(format!(
                            "qBittorrent already has \"{}\" but reports it as {} — fix or remove it there first",
                            order.title.chars().take(70).collect::<String>(),
                            existing.state
                        )));
                    }
                    // qBittorrent's own hash is the identity every later pass
                    // matches on; an indexer's claim that got us here by name
                    // was wrong
                    order.info_hash = Some(existing.hash.clone());
                    // A torrent the user stopped part-way is adoptable but
                    // would sit there forever: the medic judges only
                    // stalledDL/metaDL and the completion pass only sees
                    // "present". Start it unless grabs are meant to arrive
                    // paused; never touch a finished one (seeding policy).
                    let stopped = existing.state.starts_with("stopped") || existing.state.starts_with("paused");
                    if stopped && existing.progress < 1.0 && !cfg.add_paused {
                        if let Err(e) = q.torrent_action("start", &existing.hash).await {
                            crate::applog::warn(
                                "grab",
                                format!("adopted torrent is stopped and could not be started: {e}"),
                            );
                        } else {
                            crate::applog::info(
                                "grab",
                                format!(
                                    "adopted torrent was stopped at {:.0}% — started it",
                                    existing.progress * 100.0
                                ),
                            );
                        }
                    }
                    // Make it visible where Trawler's grabs live, but only
                    // when that cannot move anything: never overwrite a
                    // category the user chose, and never touch a torrent under
                    // Automatic Torrent Management, which relocates its files
                    // when the category changes.
                    if !cfg.qbit_category.is_empty() && existing.category.is_empty() && !existing.auto_tmm {
                        if let Err(e) = q.set_category(&existing.hash, &cfg.qbit_category).await {
                            crate::applog::warn(
                                "grab",
                                format!("adopted torrent could not be labeled with the {} category: {e}", cfg.qbit_category),
                            );
                        }
                    } else if !cfg.qbit_category.is_empty() && existing.category != cfg.qbit_category {
                        crate::applog::info(
                            "grab",
                            format!(
                                "adopted torrent keeps its own category ({}); it will not appear under Trawler's in Downloads",
                                if existing.category.is_empty() { "none" } else { existing.category.as_str() }
                            ),
                        );
                    }
                    crate::applog::info(
                        "grab",
                        format!(
                            "qBittorrent already had \"{}\" — adopting the existing torrent",
                            order.title.chars().take(70).collect::<String>()
                        ),
                    );
                }
                Ok(None)
            }
        }
        .await;
        let bp_token = match dispatch_result {
            Ok(token) => token,
            Err(error @ AppError::DispatchUncertain(_)) => {
                // The request crossed the submission boundary. Deleting the
                // claim here could duplicate a transfer whose response was
                // lost; completion passes now reconcile this pending row.
                crate::applog::warn(
                    "grab",
                    format!("keeping pending {backend} claim for \"{title}\": {error}"),
                );
                return Err(error);
            }
            Err(error) => {
                // A definitive backend failure releases the durable claim so
                // a later retry is possible. If cleanup itself fails, the
                // dispatching row intentionally remains fail-safe against an
                // accidental duplicate.
                match db::open_existing()
                    .and_then(|conn| db::ledger_abandon_dispatch(&conn, &ck, &ep_ids))
                {
                    Ok(()) => {}
                    Err(cleanup_error) => crate::applog::error(
                        "grab",
                        format!(
                            "{backend} rejected \"{title}\", and its pending ledger claim could not be cleared: {cleanup_error}"
                        ),
                    ),
                }
                return Err(error);
            }
        };
        // The durable row existed before backend I/O. If this final update
        // fails after acceptance, it stays in `dispatching`, which blocks
        // later automatic re-grabs instead of losing all evidence.
        match db::open_existing() {
            Ok(conn) => {
                // (the episodes were stamped 'grabbed' by the claim itself)
                db::ledger_finish_dispatch(
                    &conn,
                    &ck,
                    order.info_hash.as_deref(),
                    bp_token.as_deref(),
                )?;
                Ok(())
            }
            Err(e) => {
                crate::applog::error(
                    "grab",
                    format!(
                        "{backend} took \"{title}\"; its durable ledger claim remains pending because finalization failed: {e}"
                    ),
                );
                Err(e)
            }
        }
    });
    handle
        .await
        .map_err(|e| AppError::Other(format!("grab task failed: {e}")))??;
    Ok(GrabOutcome::Grabbed { backend })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn claims_are_exclusive_until_released() {
        let claims = Arc::new(GrabClaims::default());
        let g1 = claims.claim("some-key").expect("first claim succeeds");
        assert!(claims.claim("some-key").is_none(), "second claim on same key fails");
        assert!(claims.claim("other-key").is_some(), "different key unaffected");
        drop(g1);
        assert!(claims.claim("some-key").is_some(), "claim released on drop");
    }

    #[test]
    fn guard_releases_on_panic_unwind() {
        let claims = Arc::new(GrabClaims::default());
        let claims2 = Arc::clone(&claims);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _g = claims2.claim("panicky").unwrap();
            panic!("boom");
        }));
        assert!(result.is_err());
        assert!(claims.claim("panicky").is_some(), "claim released during unwind");
    }

    #[test]
    fn only_real_magnets_travel_as_magnets() {
        // an indexer definition can put an http link (passkey included) in
        // the magnet field; it must be fetched through Prowlarr like any
        // download link, never handed to Bitport or qBittorrent as a URL
        let (m, d) = split_sources(Some("magnet:?xt=urn:btih:abc".into()), Some("http://x/dl".into()));
        assert_eq!(m.as_deref(), Some("magnet:?xt=urn:btih:abc"));
        assert_eq!(d.as_deref(), Some("http://x/dl"));
        let (m, d) = split_sources(Some("MAGNET:?xt=urn:btih:abc".into()), None);
        assert!(m.is_some() && d.is_none());
        let (m, d) = split_sources(Some("https://tracker/dl?passkey=k".into()), None);
        assert!(m.is_none());
        assert_eq!(d.as_deref(), Some("https://tracker/dl?passkey=k"));
        // a real download link is not displaced by a bogus magnet
        let (m, d) = split_sources(Some("https://tracker/a".into()), Some("https://tracker/b".into()));
        assert!(m.is_none());
        assert_eq!(d.as_deref(), Some("https://tracker/b"));
        let (m, d) = split_sources(Some("".into()), Some("".into()));
        assert!(m.is_none() && d.is_none());
    }

    #[test]
    fn automatic_grabs_keep_a_five_gb_floor_whatever_the_setting() {
        let low = crate::config::Config { agent_min_free_disk_gb: 1.0, ..Default::default() };
        assert_eq!(min_free_bytes(&low), 5_000_000_000.0);
        let high = crate::config::Config { agent_min_free_disk_gb: 80.0, ..Default::default() };
        assert_eq!(min_free_bytes(&high), 80_000_000_000.0);
    }

    #[tokio::test]
    async fn bitport_capacity_does_not_probe_unavailable_qbittorrent() {
        let cfg = crate::config::Config {
            download_backend: "bitport".into(),
            bitport_token: "connected".into(),
            ..Default::default()
        };
        let qbit_called = AtomicBool::new(false);

        let free = selected_backend_free_bytes_with(
            &cfg,
            || async { Ok(40_000_000_000) },
            || async {
                qbit_called.store(true, Ordering::SeqCst);
                Err(AppError::Other("qBittorrent is stopped".into()))
            },
        )
        .await
        .unwrap();

        assert_eq!(free, 40_000_000_000);
        assert!(!qbit_called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn bitport_capacity_does_not_use_low_local_disk() {
        let cfg = crate::config::Config {
            download_backend: "bitport".into(),
            bitport_token: "connected".into(),
            ..Default::default()
        };

        let free = selected_backend_free_bytes_with(
            &cfg,
            || async { Ok(25_000_000_000) },
            || async { Ok(1) },
        )
        .await
        .unwrap();

        assert_eq!(free, 25_000_000_000);
    }
}
