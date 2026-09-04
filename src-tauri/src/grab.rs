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
                match perform_grab_core(&http, &cfg, &order, &ck).await {
                    Ok(()) => Ok(None),
                    // The torrent is already in qBittorrent (a reap that fired
                    // on a partial listing, or the user added it by hand).
                    // Abandoning the claim here made the scheduler pick the
                    // same top release again next cycle, forever. Adopt it:
                    // the ledger row finishes as grabbed and the completion
                    // pass tracks the existing torrent like any other.
                    Err(AppError::QbitDuplicate) => {
                        crate::applog::info(
                            "grab",
                            format!(
                                "qBittorrent already had \"{}\" — adopting the existing torrent",
                                order.title.chars().take(70).collect::<String>()
                            ),
                        );
                        Ok(None)
                    }
                    Err(error) => Err(error),
                }
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
