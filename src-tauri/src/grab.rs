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
use std::sync::{Arc, Mutex};

use crate::commands::perform_grab_core;
use crate::db;
use crate::error::{AppError, Result};
use crate::AppState;

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
    /// Sent to qBittorrent; ledger row written, episodes stamped.
    Grabbed,
    /// Another path is grabbing this content right now — treat as handled.
    AlreadyClaimed,
    /// The ledger already counts this content — nothing to do.
    AlreadyHad,
}

pub async fn dispatch(
    state: &AppState,
    order: GrabOrder,
    brief_id: Option<i64>,
    ep_ids: Vec<i64>,
) -> Result<GrabOutcome> {
    let ck = crate::briefs::content_key(&order.title);
    let Some(claim) = state.grab_claims.claim(&ck) else {
        return Ok(GrabOutcome::AlreadyClaimed);
    };
    {
        let conn = state.db.lock().await;
        if db::ledger_satisfied(&conn, &ck) {
            return Ok(GrabOutcome::AlreadyHad);
        }
    }
    let http = state.http.clone();
    let cfg = state.config.read().await.clone();
    let title = order.title.clone();
    // the ADDITIVE cloud backend: same claim, same ledger, same episode
    // linkage — only the transport differs. Chosen per config, never forced.
    let use_bitport = cfg.download_backend == "bitport" && !cfg.bitport_token.is_empty();
    let backend: &'static str = if use_bitport { "bitport" } else { "qbittorrent" };
    let handle = tauri::async_runtime::spawn(async move {
        let _claim = claim; // released when the grab settles, even on panic
        if use_bitport {
            // Bitport takes a magnet (or a URL to a .torrent). The magnet is
            // preferred; a Prowlarr download_url resolves to a magnet via the
            // redirect handling in fetch_torrent for most public indexers.
            let src = match order.magnet_url.clone().or_else(|| order.download_url.clone()) {
                Some(s) => s,
                None => {
                    return Err(crate::error::AppError::Other(
                        "this release offers no magnet or download link".into(),
                    ))
                }
            };
            let bp = crate::bitport::BitportClient { http: &http, token: cfg.bitport_token.clone() };
            bp.add_transfer(&src).await?;
            crate::applog::info("bitport", format!("sent to cloud: {}", order.title.chars().take(70).collect::<String>()));
        } else {
            perform_grab_core(&http, &cfg, &order).await?;
        }
        // A fresh connection, not AppState's: this task must outlive callers
        // that can be dropped mid-await, and the shared connection is not
        // 'static. If recording fails after qBittorrent accepted the add,
        // say so loudly — a silent miss here means future re-grabs.
        match db::open_existing() {
            Ok(conn) => {
                // the ledger row is the "already have" record — if it can't
                // be written, the grab must surface as failed, or every
                // later pass re-grabs this content forever
                db::ledger_insert(
                    &conn,
                    &ck,
                    brief_id,
                    &order.title,
                    order.info_hash.as_deref(),
                    order.size,
                    &ep_ids,
                    backend,
                )?;
                if !ep_ids.is_empty() {
                    if let Err(e) = db::mark_grabbed(&conn, &ep_ids, &order.title) {
                        // not fatal (the torrent is in qBittorrent and the
                        // ledger row is written) but nothing repairs a
                        // missed stamp automatically — the episode stays
                        // 'wanted' and gets re-searched each cycle
                        crate::applog::error(
                            "grab",
                            format!("ledger recorded but episode stamping failed for \"{title}\": {e}"),
                        );
                    }
                }
                Ok(())
            }
            Err(e) => {
                crate::applog::error(
                    "grab",
                    format!("{backend} took \"{title}\" but the ledger write failed: {e}"),
                );
                Err(e)
            }
        }
    });
    handle
        .await
        .map_err(|e| AppError::Other(format!("grab task failed: {e}")))??;
    Ok(GrabOutcome::Grabbed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
