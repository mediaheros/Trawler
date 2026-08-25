//! Bitport.io cloud backend: the torrenting happens on their servers, files
//! come back over plain HTTPS. An ADDITIVE download backend — local
//! qBittorrent remains the default and nothing here runs unless the user
//! connects an account in Settings.
//!
//! Shapes verified live against api.bitport.io v2 (2026-08-20):
//! - auth: OAuth2 code exchange → bearer token, scope "full", ~10y expiry
//! - POST /v2/transfers, mandatory form field literally named "torrent"
//! - GET /v2/transfers → token/name/status("finished")/substatus/progress/
//!   file_id/folder_id/src (the original magnet)
//! - GET /v2/me → plan_name, plan_expired, disk {size, available, used}

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

/// OAuth client identity for the registered "Trawler by Media Hero" app.
/// For installed (native) apps this pair is not treated as confidential —
/// it identifies the APP, never a user. The user's bearer token is the real
/// secret and lives only in their local config.
pub const CLIENT_ID: &str = "998344708";
pub const CLIENT_SECRET: &str = "9ckrekz28iqrm6mcy2";

/// The loopback port Trawler listens on to catch the OAuth redirect.
///
/// Deliberately low: every Hyper-V/WSL/Docker reserved range observed in the
/// wild sits above 28000, and a reserved port makes the listener UNBINDABLE
/// (EACCES) with no way to recover in-process. The original registered
/// callback used 53682, which landed inside 53647-53746 on exactly such a
/// machine — the whole reason this flow once demanded copy-paste.
pub const CALLBACK_PORT: u16 = 8788;

pub fn redirect_uri() -> String {
    format!("http://127.0.0.1:{CALLBACK_PORT}/bitport-callback")
}

/// The registered app credentials, overridable via env so a rotation by
/// Bitport (these are public in the MIT repo) doesn't strand installs
/// until an app update ships.
pub fn client_id() -> String {
    std::env::var("TRAWLER_BITPORT_CLIENT_ID").unwrap_or_else(|_| CLIENT_ID.into())
}
pub fn client_secret() -> String {
    std::env::var("TRAWLER_BITPORT_CLIENT_SECRET").unwrap_or_else(|_| CLIENT_SECRET.into())
}

pub fn new_oauth_state() -> Result<String> {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| AppError::Other(format!("could not create OAuth state: {e}")))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

pub fn authorize_url(state: Option<&str>) -> String {
    let redirect: String = url::form_urlencoded::byte_serialize(redirect_uri().as_bytes()).collect();
    let mut url = format!(
        "https://api.bitport.io/v2/oauth2/authorize?response_type=code&client_id={}&redirect_uri={redirect}",
        client_id()
    );
    if let Some(state) = state {
        let state: String = url::form_urlencoded::byte_serialize(state.as_bytes()).collect();
        url.push_str("&state=");
        url.push_str(&state);
    }
    url
}

/// Claim the callback port BEFORE the browser opens, so a fast approval can
/// never beat us to it — and so an unbindable port fails immediately with an
/// explanation instead of hanging until timeout.
pub async fn bind_callback() -> Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .map_err(|e| {
            let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
                " — on Windows this port is inside a system-reserved range (check: netsh interface ipv4 show excludedportrange protocol=tcp)"
            } else if e.kind() == std::io::ErrorKind::AddrInUse {
                " — something else is already using it; close it and try again"
            } else {
                ""
            };
            AppError::Other(format!("Trawler could not listen on 127.0.0.1:{CALLBACK_PORT}{hint} ({e})"))
        })
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn callback_page(ok: bool, headline: &str, detail: &str) -> String {
    let accent = if ok { "#2dd4bf" } else { "#f87171" };
    let headline = html_escape(headline);
    let detail = html_escape(detail);
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Trawler</title>\
<body style=\"margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
background:#0b0f14;color:#e6edf3;font:15px/1.5 -apple-system,Segoe UI,system-ui,sans-serif\">\
<div style=\"text-align:center;padding:40px\">\
<div style=\"font-size:34px;font-weight:600;color:{accent};margin-bottom:10px\">{headline}</div>\
<div style=\"opacity:.75\">{detail}</div></div>"
    );
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// Wait for the browser to hand back the authorization code. Ignores the
/// stray requests browsers make (favicon, prefetch) and keeps listening.
pub async fn await_code(
    listener: tokio::net::TcpListener,
    timeout: std::time::Duration,
    expected_state: &str,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let (mut sock, _) = match tokio::time::timeout_at(deadline, listener.accept()).await {
            Err(_) => {
                return Err(AppError::Other(
                    "timed out waiting for the approval in your browser — click Connect again".into(),
                ))
            }
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return Err(AppError::Other(format!("callback listener failed: {e}"))),
        };
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let target = req
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        let mut code: Option<String> = None;
        let mut denied: Option<String> = None;
        let mut returned_state: Option<String> = None;
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "error_description" => denied = Some(v.into_owned()),
                "error" => denied = denied.or_else(|| Some(v.into_owned())),
                "state" => returned_state = Some(v.into_owned()),
                _ => {}
            }
        }
        if (code.is_some() || denied.is_some())
            && returned_state.as_deref() != Some(expected_state)
        {
            let _ = sock
                .write_all(
                    callback_page(
                        false,
                        "Not connected",
                        "This approval did not belong to the current Trawler request. Return to Trawler and try again.",
                    )
                    .as_bytes(),
                )
                .await;
            let _ = sock.flush().await;
            continue;
        }
        if let Some(c) = code.filter(|c| !c.is_empty()) {
            let _ = sock
                .write_all(callback_page(true, "Connected", "Trawler has your Bitport account. You can close this tab.").as_bytes())
                .await;
            let _ = sock.flush().await;
            return Ok(c);
        }
        if let Some(d) = denied {
            let _ = sock
                .write_all(callback_page(false, "Not connected", &format!("Bitport said: {d}")).as_bytes())
                .await;
            let _ = sock.flush().await;
            return Err(AppError::Other(format!("Bitport declined the connection: {d}")));
        }
        // favicon / prefetch / bare visit — answer politely and keep waiting
        let _ = sock
            .write_all(callback_page(true, "Waiting", "Approve Trawler on the Bitport page to finish.").as_bytes())
            .await;
    }
}

const BASE: &str = "https://api.bitport.io/v2";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BitportQuota {
    pub plan_name: String,
    pub plan_expired: bool,
    pub disk_size: i64,
    pub disk_available: i64,
    pub disk_used: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BitportTransfer {
    pub token: String,
    pub name: String,
    /// observed: "finished"; defensive mapping for everything else
    pub status: String,
    pub substatus: Option<String>,
    /// their API sends this as a string (often empty); normalized 0-100
    pub progress: f64,
    pub size: Option<i64>,
    pub file_id: Option<String>,
    pub folder_id: Option<String>,
    /// the original magnet — carries the btih for ledger matching
    pub src: Option<String>,
}

pub struct BitportClient<'a> {
    pub http: &'a reqwest::Client,
    pub token: String,
}

/// Their envelope: { status, data, errors: [{message, code}] } — except the
/// token endpoint, which returns bare OAuth JSON (verified live).
fn unwrap_envelope(v: serde_json::Value) -> Result<serde_json::Value> {
    if v.get("status").and_then(|s| s.as_str()) == Some("error") {
        let msg = v
            .pointer("/errors/0/message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown Bitport error");
        return Err(AppError::Other(format!("Bitport: {msg}")));
    }
    Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null))
}

/// One-time code → long-lived bearer token (static: no client needed yet).
pub async fn exchange_code(http: &reqwest::Client, code: &str) -> Result<String> {
    let resp = http
        .post(format!("{BASE}/oauth2/access-token"))
        .form(&[
            ("client_id", client_id().as_str()),
            ("client_secret", client_secret().as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
        ])
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    // bare OAuth shape first, envelope shape as fallback
    if let Some(t) = v.get("access_token").and_then(|t| t.as_str()) {
        return Ok(t.to_string());
    }
    if let Some(t) = v.pointer("/data/access_token").and_then(|t| t.as_str()) {
        return Ok(t.to_string());
    }
    let msg = v
        .pointer("/errors/0/message")
        .and_then(|m| m.as_str())
        .unwrap_or("no access_token in response — the code may have expired; get a fresh one");
    Err(AppError::Other(format!("Bitport connect failed: {msg}")))
}

impl BitportClient<'_> {
    async fn get(&self, path: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(format!("{BASE}{path}"))
            .bearer_auth(&self.token)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await?;
        if resp.status().as_u16() == 401 {
            return Err(AppError::Other(
                "Bitport rejected the token — reconnect the account in Settings".into(),
            ));
        }
        unwrap_envelope(resp.json().await?)
    }

    pub async fn me(&self) -> Result<BitportQuota> {
        let d = self.get("/me").await?;
        Ok(BitportQuota {
            plan_name: d.get("plan_name").and_then(|v| v.as_str()).unwrap_or("?").into(),
            plan_expired: d.get("plan_expired").and_then(|v| v.as_bool()).unwrap_or(false),
            disk_size: d.pointer("/disk/size").and_then(|v| v.as_i64()).unwrap_or(0),
            disk_available: d.pointer("/disk/available").and_then(|v| v.as_i64()).unwrap_or(0),
            disk_used: d.pointer("/disk/used").and_then(|v| v.as_i64()).unwrap_or(0),
        })
    }

    pub async fn transfers(&self) -> Result<Vec<BitportTransfer>> {
        let d = self.get("/transfers").await?;
        parse_transfers(&d)
    }

    /// Submit a magnet to the cloud; returns the new transfer's token when
    /// the response carries one, so the ledger can match it exactly later.
    /// (The API also accepts URLs on this field — Trawler never sends one,
    /// because a local download_url can carry credentials.)
    pub async fn add_transfer(&self, torrent: &str) -> Result<Option<String>> {
        let resp = self
            .http
            .post(format!("{BASE}/transfers"))
            .bearer_auth(&self.token)
            .form(&[("torrent", torrent)])
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| {
                if error.is_connect() || error.is_builder() {
                    AppError::Http(error)
                } else {
                    AppError::DispatchUncertain(format!(
                        "Bitport may have accepted the transfer, but its response was lost ({error}); Trawler will reconcile it before retrying"
                    ))
                }
            })?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(bitport_add_status_error(status, &body));
        }
        let response = resp.json().await.map_err(|error| {
            AppError::DispatchUncertain(format!(
                "Bitport may have accepted the transfer, but its response could not be read ({error}); Trawler will reconcile it before retrying"
            ))
        })?;
        let d = unwrap_envelope(response)?;
        Ok(token_from_add_response(&d))
    }

    pub async fn delete_transfer(&self, token: &str) -> Result<()> {
        let resp = self
            .http
            .delete(format!("{BASE}/transfers/{token}"))
            .bearer_auth(&self.token)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await?;
        unwrap_envelope(resp.json().await?).map(|_| ())
    }
}

/// The add response's data shape isn't pinned by a live probe — pull the
/// token defensively from an object or a one-element array; None just means
/// completion matching falls back to the magnet's btih.
fn token_from_add_response(d: &serde_json::Value) -> Option<String> {
    let obj = if d.is_array() { d.as_array()?.first()? } else { d };
    obj.get("token").and_then(|v| v.as_str()).map(str::to_string)
}

fn bitport_add_status_error(status: reqwest::StatusCode, body: &str) -> AppError {
    if status == reqwest::StatusCode::REQUEST_TIMEOUT || status.is_server_error() {
        AppError::DispatchUncertain(format!(
            "Bitport may have accepted the transfer before returning {status}; Trawler will reconcile it before retrying"
        ))
    } else {
        AppError::Other(format!(
            "Bitport rejected the transfer ({status}): {}",
            body.chars().take(300).collect::<String>()
        ))
    }
}

/// Users paste either the bare authorization code or the whole redirect URL
/// from the address bar — accept both.
pub fn extract_code(input: &str) -> String {
    let s = input.trim();
    if let Some(i) = s.find("code=") {
        s[i + 5..]
            .split(['&', '#', '?'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    } else {
        s.to_string()
    }
}

fn parse_transfers(d: &serde_json::Value) -> Result<Vec<BitportTransfer>> {
    // A partially malformed listing is not authoritative absence. Silently
    // dropping one item can make reconciliation retire and duplicate the
    // corresponding live cloud transfer.
    let arr = d.as_array().ok_or_else(|| {
        AppError::Other("Bitport transfers: unexpected response shape (not a list)".into())
    })?;
    arr.iter()
        .enumerate()
        .map(|(index, value)| {
            parse_transfer(value).map_err(|error| {
                AppError::Other(format!("Bitport transfers: malformed item {index}: {error}"))
            })
        })
        .collect()
}

fn parse_transfer(v: &serde_json::Value) -> std::result::Result<BitportTransfer, &'static str> {
    let token = v
        .get("token")
        .and_then(|token| token.as_str())
        .filter(|token| !token.is_empty())
        .ok_or("missing transfer token")?;
    Ok(BitportTransfer {
        token: token.to_string(),
        name: v.get("name").and_then(|n| n.as_str()).unwrap_or("(unnamed)").to_string(),
        status: v.get("status").and_then(|s| s.as_str()).unwrap_or("unknown").to_string(),
        substatus: v.get("substatus").and_then(|s| s.as_str()).map(String::from),
        progress: parse_progress(v.get("progress")),
        size: v.get("size").and_then(|s| s.as_i64()),
        file_id: v.get("file_id").and_then(|s| s.as_str()).map(String::from),
        folder_id: v.get("folder_id").and_then(|s| s.as_str()).map(String::from),
        src: v.get("src").and_then(|s| s.as_str()).map(String::from),
    })
}

/// Their progress arrives as a string ("", "42", maybe "42%") or a number.
/// A finished transfer with an empty progress string counts as 100.
fn parse_progress(v: Option<&serde_json::Value>) -> f64 {
    match v {
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0).clamp(0.0, 100.0),
        Some(serde_json::Value::String(s)) => s
            .trim()
            .trim_end_matches('%')
            .parse::<f64>()
            .unwrap_or(0.0)
            .clamp(0.0, 100.0),
        _ => 0.0,
    }
}

/// The btih out of a transfer's src magnet, for ledger matching.
pub fn transfer_hash(t: &BitportTransfer) -> Option<String> {
    crate::scheduler::magnet_hash(t.src.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_response_token_and_pasted_code_parse() {
        use serde_json::json;
        assert_eq!(token_from_add_response(&json!({"token": "abC1", "name": "x"})).as_deref(), Some("abC1"));
        assert_eq!(token_from_add_response(&json!([{"token": "t2"}])).as_deref(), Some("t2"));
        assert_eq!(token_from_add_response(&json!({"ok": true})), None);
        assert_eq!(extract_code("04ccde79"), "04ccde79");
        assert_eq!(
            extract_code("http://127.0.0.1:28688/bitport-callback?code=04ccde79&state=x"),
            "04ccde79"
        );
        assert_eq!(extract_code("  code=abc#frag  "), "abc");
    }

    #[test]
    fn oauth_url_carries_state_and_callback_text_is_escaped() {
        let url = authorize_url(Some("state with + symbols"));
        assert!(url.contains("state=state+with+%2B+symbols"));
        let page = callback_page(false, "No <script>", "bad & \"worse\"");
        assert!(!page.contains("<script>"));
        assert!(page.contains("No &lt;script&gt;"));
        assert!(page.contains("bad &amp; &quot;worse&quot;"));
    }

    #[test]
    fn progress_parses_defensively() {
        use serde_json::json;
        assert_eq!(parse_progress(Some(&json!(""))), 0.0);
        assert_eq!(parse_progress(Some(&json!("42"))), 42.0);
        assert_eq!(parse_progress(Some(&json!("87%"))), 87.0);
        assert_eq!(parse_progress(Some(&json!(63.5))), 63.5);
        assert_eq!(parse_progress(Some(&json!("150"))), 100.0);
        assert_eq!(parse_progress(None), 0.0);
    }

    #[test]
    fn malformed_transfer_items_fail_the_whole_listing() {
        use serde_json::json;
        let listing = json!([
            {"token": "valid", "name": "one", "status": "downloading"},
            {"name": "missing identity", "status": "downloading"}
        ]);
        let error = parse_transfers(&listing).unwrap_err();
        assert!(error.to_string().contains("malformed item 1"));
        assert!(error.to_string().contains("missing transfer token"));
    }

    #[test]
    fn ambiguous_bitport_statuses_keep_the_durable_claim() {
        assert!(matches!(
            bitport_add_status_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "oops"),
            AppError::DispatchUncertain(_)
        ));
        assert!(matches!(
            bitport_add_status_error(reqwest::StatusCode::REQUEST_TIMEOUT, "late"),
            AppError::DispatchUncertain(_)
        ));
        assert!(matches!(
            bitport_add_status_error(reqwest::StatusCode::BAD_REQUEST, "bad magnet"),
            AppError::Other(_)
        ));
    }

    #[test]
    fn transfer_parses_the_live_shape() {
        // verbatim shape from the live API (2026-08-20), values anonymized
        let v = serde_json::json!({
            "token": "oaMUxTH-XX",
            "name": "Some.Show.S01E01.720p.mkv",
            "status": "finished",
            "substatus": null,
            "size": null,
            "message": null,
            "progress": "",
            "folder_id": null,
            "file_id": "-ZtMIVBW-XX",
            "other_cloud_id": null,
            "src": "magnet:?xt=urn:btih:7e6183491295ab408d417b2b91c352b41703b2ed&dn=x"
        });
        let t = parse_transfer(&v).expect("parses");
        assert_eq!(t.status, "finished");
        assert_eq!(t.file_id.as_deref(), Some("-ZtMIVBW-XX"));
        assert_eq!(
            transfer_hash(&t).as_deref(),
            Some("7e6183491295ab408d417b2b91c352b41703b2ed")
        );
    }

    #[test]
    fn envelope_errors_surface() {
        let err = serde_json::json!({
            "status": "error", "data": null,
            "errors": [{"message": "Parameter torrent is mandatory.", "code": 101}]
        });
        let e = unwrap_envelope(err).unwrap_err();
        assert!(e.to_string().contains("Parameter torrent is mandatory"));
    }
}
