//! Bitport.io API client: the torrenting happens on their servers, finished
//! files come back to this machine over plain HTTPS (see `cloud.rs` for the
//! poller and the fetcher). An ADDITIVE download backend — local qBittorrent
//! remains the default and nothing here runs unless the user connects an
//! account in Settings.
//!
//! Shapes verified live against api.bitport.io v2 (2026-09-12):
//! - auth: the redirect carries ONLY `code` — no `state` echo — and the
//!   token endpoint answers HTTP 200 with a bare `{error, error_description}`
//!   on failure; success is bare `{access_token, expires_in (~10y), scope}`
//! - POST /v2/transfers takes a form field literally named `torrent` that
//!   must be a URL or a magnet (multipart .torrent uploads are refused with
//!   code 102) and answers with an EMPTY data array — no token
//! - GET /v2/transfers is the whole account history, newest first, no
//!   pagination; status ∈ {queued, downloading, finished, seeding, error},
//!   `progress` is a string ("42.5", "" when finished), `message` explains
//!   errors; a finished transfer points at exactly one of file_id/folder_id
//! - GET /v2/cloud/<folder>?scope=recursive returns the full tree with a
//!   signed `download_url` per file (7-day expiry, no auth, Range works)
//! - GET /v2/files/<code> adds `crc32`; DELETE /v2/transfers/<token> also
//!   deletes the files and frees the quota
//! - error envelopes arrive as HTTP 200 `{status:"error", errors:[…]}`;
//!   only a bad token yields a real 401

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

/// OAuth client identity for the registered "Trawler by Media Hero" app.
/// For installed (native) apps this pair is not treated as confidential —
/// it identifies the APP, never a user. The user's bearer token is the real
/// secret and lives only in their local config.
pub const CLIENT_ID: &str = "998344708";
pub const CLIENT_SECRET: &str = "bafn4mdre7ap3hk6q2";

/// The loopback port Trawler listens on to catch the OAuth redirect.
///
/// Deliberately low: every Hyper-V/WSL/Docker reserved range observed in the
/// wild sits above 28000, and a reserved port makes the listener UNBINDABLE
/// (EACCES) with no way to recover in-process. The original registered
/// callback used 53682, which landed inside 53647-53746 on exactly such a
/// machine — the whole reason this flow once demanded copy-paste.
pub const CALLBACK_PORT: u16 = 8788;

/// Where a user on another machine gets a one-time code by hand.
pub const GET_ACCESS_URL: &str = "https://bitport.io/get-access";

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

/// The verdict on one callback request, separated from the socket handling
/// so the state rule can be tested.
#[derive(Debug, PartialEq)]
enum CallbackVerdict {
    /// favicon / prefetch / bare visit: keep listening
    Noise,
    /// a state was echoed and it is not ours
    ForeignState,
    Code(String),
    Denied(String),
}

/// Bitport does not echo `state` (verified live): a callback WITHOUT one is
/// the normal case and must be accepted. A callback that does carry a state
/// is checked strictly — anything but ours belongs to another request.
fn judge_callback(query: &str, expected_state: &str) -> CallbackVerdict {
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
    if code.is_none() && denied.is_none() {
        return CallbackVerdict::Noise;
    }
    if let Some(state) = returned_state {
        if state != expected_state {
            return CallbackVerdict::ForeignState;
        }
    }
    if let Some(c) = code.filter(|c| !c.trim().is_empty()) {
        return CallbackVerdict::Code(c.trim().to_string());
    }
    CallbackVerdict::Denied(denied.unwrap_or_else(|| "no code returned".into()))
}

/// Without a `state` echo the only thing separating Bitport's redirect from
/// a script on some local page is the browser's own fetch metadata: a
/// top-level navigation carries `Sec-Fetch-Mode: navigate` and
/// `Sec-Fetch-Dest: document`, while `fetch()`, `<img>` and `<script>`
/// probes carry cors/no-cors and image/script/empty. Headers absent (an
/// old browser, curl) are accepted — this is a hurdle, not a proof.
fn looks_like_navigation(request: &str) -> bool {
    // a redirect is always a GET; a POST body could otherwise smuggle
    // header-looking lines, so the body is never read as headers either
    if !request.starts_with("GET ") {
        return false;
    }
    let headers = request.split("\r\n\r\n").next().unwrap_or(request);
    let headers = headers.split("\n\n").next().unwrap_or(headers);
    let mut mode: Option<String> = None;
    let mut dest: Option<String> = None;
    for line in headers.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else { continue };
        match name.trim().to_ascii_lowercase().as_str() {
            "sec-fetch-mode" => mode = Some(value.trim().to_ascii_lowercase()),
            "sec-fetch-dest" => dest = Some(value.trim().to_ascii_lowercase()),
            _ => {}
        }
    }
    let mode_ok = mode.as_deref().is_none_or(|m| m == "navigate");
    let dest_ok = dest.as_deref().is_none_or(|d| d == "document");
    mode_ok && dest_ok
}

/// Read one HTTP request head (through the blank line), bounded in size and
/// time. A single `read` may return only the first segment; the browser
/// does not retry, so a truncated request line would strand the flow.
async fn read_request_head(sock: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt;
    const LIMIT: usize = 16 * 1024;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = match tokio::time::timeout_at(deadline, sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return None,
        };
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.windows(2).any(|w| w == b"\n\n") {
            break;
        }
        if buf.len() >= LIMIT {
            break;
        }
    }
    if buf.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Wait for the browser to hand back the authorization code. Ignores the
/// stray requests browsers make (favicon, prefetch) and keeps listening.
pub async fn await_code(
    listener: tokio::net::TcpListener,
    timeout: std::time::Duration,
    expected_state: &str,
) -> Result<String> {
    use tokio::io::AsyncWriteExt;
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
        // a connection that never sends (a browser's speculative preconnect,
        // a port scanner) must not park the whole flow — and the listener
        // with it — until the app restarts
        let Some(req) = read_request_head(&mut sock).await else { continue };
        let target = req
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        if !looks_like_navigation(&req) {
            // a fetch()/<img> from some page on this machine, not the
            // browser following Bitport's redirect — never act on its code
            let _ = sock
                .write_all(callback_page(false, "Ignored", "This request did not come from a browser navigation.").as_bytes())
                .await;
            continue;
        }
        match judge_callback(query, expected_state) {
            CallbackVerdict::ForeignState => {
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
            CallbackVerdict::Code(c) => {
                let _ = sock
                    .write_all(
                        callback_page(true, "Approved", "Return to Trawler — it is finishing the connection. You can close this tab.")
                            .as_bytes(),
                    )
                    .await;
                let _ = sock.flush().await;
                return Ok(c);
            }
            CallbackVerdict::Denied(d) => {
                let _ = sock
                    .write_all(callback_page(false, "Not connected", &format!("Bitport said: {d}")).as_bytes())
                    .await;
                let _ = sock.flush().await;
                return Err(AppError::Other(format!("Bitport declined the connection: {d}")));
            }
            CallbackVerdict::Noise => {
                let _ = sock
                    .write_all(
                        callback_page(true, "Waiting", "Approve Trawler on the Bitport page to finish.").as_bytes(),
                    )
                    .await;
            }
        }
    }
}

const BASE: &str = "https://api.bitport.io/v2";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BitportQuota {
    /// the account e-mail — shown after connecting so a hijacked callback
    /// (a code for someone else's account) is visible at a glance
    pub account: Option<String>,
    pub plan_name: String,
    pub plan_expired: bool,
    /// "YYYY-MM-DD HH:MM:SS" in UTC, as Bitport reports it
    pub plan_expiration: Option<String>,
    pub disk_size: i64,
    pub disk_available: i64,
    pub disk_used: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BitportTransfer {
    pub token: String,
    pub name: String,
    /// queued | downloading | finished | seeding | error (documented); kept
    /// as a string so an undocumented value still renders
    pub status: String,
    pub substatus: Option<String>,
    /// their API sends this as a string (often empty); normalized 0-100
    pub progress: f64,
    pub size: Option<i64>,
    /// status or error text — the only explanation Bitport gives for `error`
    pub message: Option<String>,
    pub file_id: Option<String>,
    pub folder_id: Option<String>,
    /// the original magnet — carries the btih for ledger matching
    pub src: Option<String>,
}

impl BitportTransfer {
    pub fn is_finished(&self) -> bool {
        // seeding only happens after the payload is complete
        self.status == "finished" || self.status == "seeding"
    }
    pub fn is_error(&self) -> bool {
        self.status == "error"
    }
}

/// One file inside the cloud, as the folder listing and the file-info
/// endpoint describe it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CloudFile {
    pub code: String,
    pub name: String,
    pub size: i64,
    /// video | audio | image | text | application | … (Bitport's own class)
    pub kind: String,
    /// signed CDN link; valid ~7 days, no auth, Range-capable
    pub download_url: Option<String>,
    /// only the file-info endpoint fills this in
    pub crc32: Option<String>,
    /// Bitport's scanner verdict; anything but 0 is flagged
    pub virus: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CloudFolder {
    pub code: Option<String>,
    pub name: String,
    pub files: Vec<CloudFile>,
    pub folders: Vec<CloudFolder>,
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
        let code = v.pointer("/errors/0/code").and_then(|c| c.as_i64()).unwrap_or(0);
        if code == 401 || msg.eq_ignore_ascii_case("Unauthorized access") {
            return Err(AppError::BitportAuth);
        }
        return Err(AppError::Other(format!("Bitport: {msg}")));
    }
    Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null))
}

/// The envelope wraps most payloads in a one-element array; unwrap it so
/// callers see the object itself.
fn first_item(d: &serde_json::Value) -> &serde_json::Value {
    match d.as_array() {
        Some(arr) => arr.first().unwrap_or(d),
        None => d,
    }
}

/// Which grant a pasted code needs. The redirect (or a pasted redirect URL)
/// yields an authorization code; bitport.io/get-access yields a USER code
/// that the docs exchange with `grant_type=code`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CodeKind {
    Authorization,
    UserCode,
}

impl CodeKind {
    fn grant_type(self) -> &'static str {
        match self {
            CodeKind::Authorization => "authorization_code",
            CodeKind::UserCode => "code",
        }
    }
}

/// One-time code → long-lived bearer token (static: no client needed yet).
pub async fn exchange_code(http: &reqwest::Client, code: &str) -> Result<String> {
    exchange(http, code, CodeKind::Authorization).await
}

pub async fn exchange(http: &reqwest::Client, code: &str, kind: CodeKind) -> Result<String> {
    let resp = http
        .post(format!("{BASE}/oauth2/access-token"))
        .form(&[
            ("client_id", client_id().as_str()),
            ("client_secret", client_secret().as_str()),
            ("grant_type", kind.grant_type()),
            ("code", code),
        ])
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    token_from_exchange(&v)
}

/// Success is bare OAuth JSON; failure is HTTP 200 with a bare
/// `{error, error_description}` (verified live: "Invalid Client Secret"
/// arrived exactly like that). The envelope shape is kept as a fallback.
fn token_from_exchange(v: &serde_json::Value) -> Result<String> {
    if let Some(t) = v.get("access_token").and_then(|t| t.as_str()) {
        return Ok(t.to_string());
    }
    if let Some(t) = v.pointer("/data/access_token").and_then(|t| t.as_str()) {
        return Ok(t.to_string());
    }
    let msg = v
        .get("error_description")
        .and_then(|m| m.as_str())
        .or_else(|| v.get("error").and_then(|m| m.as_str()))
        .or_else(|| v.pointer("/errors/0/message").and_then(|m| m.as_str()))
        .unwrap_or("no access_token in response — the code may have expired; get a fresh one");
    Err(AppError::Other(format!("Bitport connect failed: {msg}")))
}

impl BitportClient<'_> {
    async fn get(&self, path: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(format!("{BASE}{path}"))
            .bearer_auth(&self.token)
            .timeout(std::time::Duration::from_secs(25))
            .send()
            .await?;
        if resp.status().as_u16() == 401 {
            return Err(AppError::BitportAuth);
        }
        unwrap_envelope(resp.json().await?)
    }

    pub async fn me(&self) -> Result<BitportQuota> {
        let d = self.get("/me").await?;
        Ok(parse_quota(&d))
    }

    pub async fn transfers(&self) -> Result<Vec<BitportTransfer>> {
        let d = self.get("/transfers").await?;
        parse_transfers(&d)
    }

    /// A folder and, with `recursive`, everything below it. Per-transfer
    /// listings are the only sane way in: the account-wide recursive tree
    /// was 1.4 MB on a real account.
    pub async fn folder(&self, code: &str, recursive: bool) -> Result<CloudFolder> {
        let scope = if recursive { "?scope=recursive" } else { "" };
        let d = self.get(&format!("/cloud/{code}{scope}")).await?;
        parse_folder(first_item(&d)).ok_or_else(|| {
            AppError::Other("Bitport folder listing: unexpected response shape".into())
        })
    }

    /// File metadata including `crc32` and a fresh signed download link.
    pub async fn file_info(&self, code: &str) -> Result<CloudFile> {
        let d = self.get(&format!("/files/{code}")).await?;
        parse_file(first_item(&d))
            .ok_or_else(|| AppError::Other("Bitport file info: unexpected response shape".into()))
    }

    /// Submit a magnet (or a public .torrent URL) to the cloud. The response
    /// carries no identity (verified: empty data array), so the caller finds
    /// the new transfer in the listing by its btih.
    /// (Trawler never sends a download_url from an indexer — it can carry
    /// credentials — but it will send a magnet it built from .torrent bytes.)
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
                // no request URL in any of these messages: they reach toasts,
                // the activity feed and the agent
                let error = error.without_url();
                if error.is_connect() || error.is_builder() {
                    AppError::Http(error)
                } else {
                    AppError::DispatchUncertain(format!(
                        "Bitport may have accepted the transfer, but its response was lost ({error}); Trawler will reconcile it before retrying"
                    ))
                }
            })?;
        if resp.status().as_u16() == 401 {
            return Err(AppError::BitportAuth);
        }
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

    /// Stops the transfer AND deletes its files (verified live: the folder
    /// is gone and the quota is freed).
    pub async fn delete_transfer(&self, token: &str) -> Result<()> {
        let resp = self
            .http
            .delete(format!("{BASE}/transfers/{token}"))
            .bearer_auth(&self.token)
            .timeout(std::time::Duration::from_secs(25))
            .send()
            .await?;
        if resp.status().as_u16() == 401 {
            return Err(AppError::BitportAuth);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        // a gateway error page is not "Returns an empty response on success"
        if !status.is_success() {
            return Err(AppError::Other(format!(
                "Bitport answered {status} to the delete: {}",
                text.chars().take(200).collect::<String>()
            )));
        }
        if text.trim().is_empty() {
            return Ok(());
        }
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => unwrap_envelope(v).map(|_| ()),
            Err(_) => Err(AppError::Other("Bitport answered the delete with something other than JSON".into())),
        }
    }
}

/// An integer that may arrive as a JSON number or as a numeric string.
fn json_i64(v: Option<&serde_json::Value>) -> Option<i64> {
    match v? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn parse_quota(d: &serde_json::Value) -> BitportQuota {
    BitportQuota {
        account: d.get("email").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from),
        plan_name: d.get("plan_name").and_then(|v| v.as_str()).unwrap_or("?").into(),
        plan_expired: d.get("plan_expired").and_then(|v| v.as_bool()).unwrap_or(false),
        plan_expiration: d
            .pointer("/plan_expiration/date")
            .and_then(|v| v.as_str())
            .map(String::from),
        disk_size: d.pointer("/disk/size").and_then(|v| v.as_i64()).unwrap_or(0),
        disk_available: d.pointer("/disk/available").and_then(|v| v.as_i64()).unwrap_or(0),
        disk_used: d.pointer("/disk/used").and_then(|v| v.as_i64()).unwrap_or(0),
    }
}

/// The add response's data shape is an empty array in practice — pull a
/// token defensively from an object or a one-element array anyway; None
/// just means completion matching uses the magnet's btih.
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
        // their 4xx bodies are envelopes: surface the message, not the JSON
        let msg = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.pointer("/errors/0/message").and_then(|m| m.as_str()).map(String::from))
            .unwrap_or_else(|| body.chars().take(300).collect());
        AppError::Other(format!("Bitport rejected the transfer ({status}): {msg}"))
    }
}

/// Users paste either the bare code or the whole redirect URL from the
/// address bar — accept both.
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
    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("unknown").to_string();
    let mut progress = parse_progress(v.get("progress"));
    if (status == "finished" || status == "seeding") && progress == 0.0 {
        progress = 100.0;
    }
    Ok(BitportTransfer {
        token: token.to_string(),
        name: v.get("name").and_then(|n| n.as_str()).unwrap_or("(unnamed)").to_string(),
        status,
        substatus: v.get("substatus").and_then(|s| s.as_str()).map(String::from),
        progress,
        size: v.get("size").and_then(|s| s.as_i64()),
        message: v.get("message").and_then(|s| s.as_str()).filter(|s| !s.is_empty()).map(String::from),
        file_id: v.get("file_id").and_then(|s| s.as_str()).map(String::from),
        folder_id: v.get("folder_id").and_then(|s| s.as_str()).map(String::from),
        src: v.get("src").and_then(|s| s.as_str()).map(String::from),
    })
}

fn parse_file(v: &serde_json::Value) -> Option<CloudFile> {
    let code = v.get("code")?.as_str().filter(|c| !c.is_empty())?.to_string();
    Some(CloudFile {
        code,
        name: v.get("name").and_then(|n| n.as_str()).unwrap_or("(unnamed)").to_string(),
        size: json_i64(v.get("size")).unwrap_or(0),
        kind: v.get("type").and_then(|s| s.as_str()).unwrap_or("other").to_string(),
        download_url: v.get("download_url").and_then(|s| s.as_str()).filter(|s| !s.is_empty()).map(String::from),
        crc32: v
            .get("crc32")
            .and_then(|s| s.as_str())
            .filter(|s| s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(|s| s.to_ascii_lowercase()),
        virus: json_i64(v.get("virus")).unwrap_or(0),
    })
}

fn parse_folder(v: &serde_json::Value) -> Option<CloudFolder> {
    let name = v.get("name")?.as_str().unwrap_or("(unnamed)").to_string();
    Some(CloudFolder {
        code: v.get("code").and_then(|c| c.as_str()).map(String::from),
        name,
        files: v
            .get("files")
            .and_then(|f| f.as_array())
            .map(|arr| arr.iter().filter_map(parse_file).collect())
            .unwrap_or_default(),
        folders: v
            .get("folders")
            .and_then(|f| f.as_array())
            .map(|arr| arr.iter().filter_map(parse_folder).collect())
            .unwrap_or_default(),
    })
}

/// Their progress arrives as a string ("", "42", maybe "42%") or a number.
fn parse_progress(v: Option<&serde_json::Value>) -> f64 {
    let raw = match v {
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(serde_json::Value::String(s)) => s.trim().trim_end_matches('%').parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    // "nan" parses; serde would send it as null and the view would choke
    if raw.is_finite() { raw.clamp(0.0, 100.0) } else { 0.0 }
}

/// The btih out of a transfer's src magnet, for ledger matching. Transfers
/// that came from a .torrent URL carry the bare 40-hex hash as `src`.
pub fn transfer_hash(t: &BitportTransfer) -> Option<String> {
    let src = t.src.as_deref()?;
    if let Some(h) = crate::scheduler::magnet_hash(Some(src)) {
        return Some(h);
    }
    let s = src.trim();
    if s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(s.to_ascii_lowercase());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_response_token_and_pasted_code_parse() {
        use serde_json::json;
        assert_eq!(token_from_add_response(&json!({"token": "abC1", "name": "x"})).as_deref(), Some("abC1"));
        assert_eq!(token_from_add_response(&json!([{"token": "t2"}])).as_deref(), Some("t2"));
        assert_eq!(token_from_add_response(&json!([[]])), None, "the live shape");
        assert_eq!(extract_code("04ccde79"), "04ccde79");
        assert_eq!(
            extract_code("http://127.0.0.1:8788/bitport-callback?code=04ccde79&state=x"),
            "04ccde79"
        );
        assert_eq!(extract_code("  code=abc#frag  "), "abc");
    }

    #[test]
    fn callback_without_state_is_accepted_and_foreign_state_is_not() {
        // Bitport never echoes state (live, 2026-09-12): the plain callback
        // must connect, or the one-click flow waits forever
        assert_eq!(
            judge_callback("code=add0711d", "expected"),
            CallbackVerdict::Code("add0711d".into())
        );
        assert_eq!(
            judge_callback("code=abc&state=expected", "expected"),
            CallbackVerdict::Code("abc".into())
        );
        assert_eq!(judge_callback("code=abc&state=other", "expected"), CallbackVerdict::ForeignState);
        assert_eq!(judge_callback("", "expected"), CallbackVerdict::Noise);
        assert_eq!(judge_callback("utm=1", "expected"), CallbackVerdict::Noise);
        assert_eq!(
            judge_callback("error=access_denied&error_description=User+said+no", "expected"),
            CallbackVerdict::Denied("User said no".into())
        );
    }

    #[test]
    fn only_browser_navigations_may_deliver_a_code() {
        let nav = "GET /bitport-callback?code=x HTTP/1.1\r\nHost: 127.0.0.1:8788\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\nSec-Fetch-Site: cross-site\r\n\r\n";
        assert!(looks_like_navigation(nav));
        let plain = "GET /bitport-callback?code=x HTTP/1.1\r\nHost: 127.0.0.1:8788\r\n\r\n";
        assert!(looks_like_navigation(plain), "no metadata at all is accepted");
        let fetch = "GET /bitport-callback?code=x HTTP/1.1\r\nSec-Fetch-Mode: no-cors\r\nSec-Fetch-Dest: empty\r\n\r\n";
        assert!(!looks_like_navigation(fetch));
        let img = "GET /bitport-callback?code=x HTTP/1.1\r\nsec-fetch-mode: no-cors\r\nsec-fetch-dest: image\r\n\r\n";
        assert!(!looks_like_navigation(img));
        // a POST body cannot smuggle header-looking lines past the check
        let smuggled = "POST /bitport-callback?code=x HTTP/1.1\r\nSec-Fetch-Mode: no-cors\r\nSec-Fetch-Dest: empty\r\n\r\nsec-fetch-mode: navigate\r\nsec-fetch-dest: document";
        assert!(!looks_like_navigation(smuggled));
        let get_with_body = "GET /bitport-callback?code=x HTTP/1.1\r\nSec-Fetch-Mode: no-cors\r\n\r\nsec-fetch-mode: navigate";
        assert!(!looks_like_navigation(get_with_body));
        assert_eq!(parse_progress(Some(&serde_json::json!("nan"))), 0.0);
        assert_eq!(parse_progress(Some(&serde_json::json!("inf"))), 0.0);
    }

    #[test]
    fn exchange_errors_arrive_bare_and_surface_verbatim() {
        use serde_json::json;
        let err = token_from_exchange(&json!({"error": "invalid_parameter", "error_description": "Invalid Client Secret"}))
            .unwrap_err();
        assert!(err.to_string().contains("Invalid Client Secret"), "{err}");
        assert_eq!(
            token_from_exchange(&json!({"access_token": "tok", "expires_in": 315360000, "token_type": "bearer", "scope": "full"}))
                .unwrap(),
            "tok"
        );
        assert_eq!(token_from_exchange(&json!({"data": {"access_token": "t2"}})).unwrap(), "t2");
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
        assert_eq!(parse_progress(Some(&json!("42.5"))), 42.5);
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
        let rejected = bitport_add_status_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"status":"error","data":null,"errors":[{"message":"Parameter torrent must be url or magnet.","code":102}]}"#,
        );
        assert!(matches!(rejected, AppError::Other(_)));
        assert!(rejected.to_string().contains("must be url or magnet"), "{rejected}");
    }

    #[test]
    fn transfer_parses_the_live_shapes() {
        // verbatim shapes from the live API, values anonymized
        let finished = serde_json::json!({
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
        let t = parse_transfer(&finished).expect("parses");
        assert!(t.is_finished());
        assert_eq!(t.progress, 100.0, "an empty progress string on a finished transfer reads as complete");
        assert_eq!(t.file_id.as_deref(), Some("-ZtMIVBW-XX"));
        assert_eq!(transfer_hash(&t).as_deref(), Some("7e6183491295ab408d417b2b91c352b41703b2ed"));

        let downloading = serde_json::json!({
            "token": "1234567", "name": "Big.Buck.Bunny", "status": "downloading", "substatus": null,
            "size": null, "message": null, "progress": "42.5", "folder_id": null, "file_id": null,
            "other_cloud_id": null, "src": "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c"
        });
        let t = parse_transfer(&downloading).expect("parses");
        assert!(!t.is_finished());
        assert_eq!(t.progress, 42.5);

        let errored = serde_json::json!({
            "token": "x", "name": "Broken", "status": "error", "message": "No peers found", "progress": "0",
            "src": "275a9db4830a486c8661275a9db4830a486c8661"
        });
        let t = parse_transfer(&errored).expect("parses");
        assert!(t.is_error());
        assert_eq!(t.message.as_deref(), Some("No peers found"));
        assert_eq!(
            transfer_hash(&t).as_deref(),
            Some("275a9db4830a486c8661275a9db4830a486c8661"),
            "a .torrent-sourced transfer carries the bare hash as src"
        );
    }

    #[test]
    fn folder_tree_and_file_info_parse() {
        let listing = serde_json::json!([{
            "name": "Sintel", "code": "j3Q9aHELxSiyC-yQ2Ckljg", "size": null, "files_count": null,
            "files": [
                {"name": "Sintel.mp4", "code": "f1", "size": 129241752, "type": "video", "virus": 0, "crc32": null,
                 "download_url": "https://x-sto.energycdn.com/dl/K/1789824116/800424038/6a/Sintel.mp4"},
                {"name": "poster.jpg", "code": "f2", "size": 46115, "type": "image", "virus": 0}
            ],
            "folders": [
                {"name": "Subs", "code": "d1", "files": [{"name": "Sintel.en.srt", "code": "f3", "size": 1514, "type": "text"}], "folders": []}
            ]
        }]);
        let folder = parse_folder(first_item(&listing)).expect("parses");
        assert_eq!(folder.name, "Sintel");
        assert_eq!(folder.files.len(), 2);
        assert_eq!(folder.files[0].kind, "video");
        assert!(folder.files[0].download_url.is_some());
        assert_eq!(folder.files[0].crc32, None);
        assert_eq!(folder.folders[0].files[0].name, "Sintel.en.srt");

        let info = serde_json::json!([{"name": "poster.jpg", "code": "f2", "size": 46115, "type": "image", "crc32": "704EC3C2", "virus": 0}]);
        let file = parse_file(first_item(&info)).expect("parses");
        assert_eq!(file.crc32.as_deref(), Some("704ec3c2"));

        assert_eq!(json_i64(Some(&serde_json::json!("129241752"))), Some(129241752));
        assert_eq!(json_i64(Some(&serde_json::json!(7))), Some(7));
        assert_eq!(json_i64(Some(&serde_json::json!(null))), None);

        let quota = parse_quota(&serde_json::json!({
            "email": "you@example.com",
            "plan_name": "big", "plan_expired": false,
            "plan_expiration": {"date": "2027-08-16 00:00:00", "timezone_type": 3, "timezone": "UTC"},
            "disk": {"size": 1073741824000i64, "available": 343501989179i64, "used": 730239834821i64}
        }));
        assert_eq!(quota.plan_expiration.as_deref(), Some("2027-08-16 00:00:00"));
        assert_eq!(quota.account.as_deref(), Some("you@example.com"));
        assert_eq!(quota.disk_available, 343501989179);
    }

    #[test]
    fn envelope_errors_surface_and_auth_is_distinct() {
        let err = serde_json::json!({
            "status": "error", "data": null,
            "errors": [{"message": "Parameter torrent is mandatory.", "code": 101}]
        });
        let e = unwrap_envelope(err).unwrap_err();
        assert!(e.to_string().contains("Parameter torrent is mandatory"));
        let auth = serde_json::json!({"status": "error", "data": null, "errors": [{"message": "Unauthorized access", "code": 401}]});
        assert!(matches!(unwrap_envelope(auth).unwrap_err(), AppError::BitportAuth));
    }
}
