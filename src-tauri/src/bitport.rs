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

pub fn authorize_url() -> String {
    format!("https://api.bitport.io/v2/oauth2/authorize?response_type=code&client_id={CLIENT_ID}")
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
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
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
        Ok(d.as_array()
            .map(|a| a.iter().filter_map(parse_transfer).collect())
            .unwrap_or_default())
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
            .await?;
        let d = unwrap_envelope(resp.json().await?)?;
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

fn parse_transfer(v: &serde_json::Value) -> Option<BitportTransfer> {
    Some(BitportTransfer {
        token: v.get("token")?.as_str()?.to_string(),
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
