use serde::{Deserialize, Serialize};
use crate::error::{AppError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QbitTorrent {
    pub hash: String,
    pub name: String,
    pub size: i64,
    pub progress: f64,
    pub dlspeed: i64,
    pub upspeed: i64,
    pub eta: i64,
    pub state: String,
    #[serde(default)]
    pub category: String,
    pub save_path: String,
    pub added_on: i64,
    #[serde(default)]
    pub num_seeds: i64,
    #[serde(default)]
    pub num_leechs: i64,
    #[serde(default)]
    pub ratio: f64,
    #[serde(default)]
    pub content_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferInfo {
    pub dl_info_speed: i64,
    pub up_info_speed: i64,
    #[serde(default)]
    pub dl_info_data: i64,
    #[serde(default)]
    pub up_info_data: i64,
}

pub struct QbitClient<'a> {
    pub http: &'a reqwest::Client, // built with cookie_store(true)
    pub base: String,
    pub username: String,
    pub password: String,
}

pub struct AddTorrent<'a> {
    pub magnet: Option<String>,
    pub torrent_bytes: Option<Vec<u8>>,
    pub torrent_name: &'a str,
    pub save_path: Option<String>,
    pub category: Option<String>,
    pub paused: bool,
    /// qBt share limit: None = client default (-2), Some(0.0) = stop when
    /// complete, Some(r) = stop at ratio r
    pub ratio_limit: Option<f64>,
}

impl<'a> QbitClient<'a> {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }

    /// qBt CSRF protection wants a matching Referer/Origin on POSTs.
    fn referer(&self) -> String {
        self.base.trim_end_matches('/').to_string()
    }

    pub async fn login(&self) -> Result<()> {
        use std::sync::atomic::{AtomicI64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        // bad credentials + the 2s Downloads poll would otherwise hammer
        // /auth/login until qBittorrent bans this IP — including localhost
        static BLOCKED_UNTIL: AtomicI64 = AtomicI64::new(0);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        if now < BLOCKED_UNTIL.load(Ordering::Relaxed) {
            return Err(AppError::QbitAuth);
        }
        let resp = self
            .http
            .post(self.url("/api/v2/auth/login"))
            .header("Referer", self.referer())
            .form(&[("username", &self.username), ("password", &self.password)])
            .send()
            .await?;
        let ok = resp.status().is_success();
        let body = resp.text().await.unwrap_or_default();
        // qBt answers 200 "Fails." on bad credentials.
        if !ok || body.contains("Fails") {
            BLOCKED_UNTIL.store(now + 45, Ordering::Relaxed);
            return Err(AppError::QbitAuth);
        }
        BLOCKED_UNTIL.store(0, Ordering::Relaxed);
        Ok(())
    }

    /// GET an endpoint; on 401/403 try logging in once and retry.
    async fn get_authed(&self, path: &str, query: &[(&str, String)]) -> Result<reqwest::Response> {
        for attempt in 0..2 {
            let resp = self
                .http
                .get(self.url(path))
                .header("Referer", self.referer())
                .query(query)
                .send()
                .await?;
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                if attempt == 0 {
                    self.login().await?;
                    continue;
                }
                return Err(AppError::QbitAuth);
            }
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(AppError::Qbit { status, body: body.chars().take(300).collect() });
            }
            return Ok(resp);
        }
        unreachable!()
    }

    async fn post_authed(&self, path: &str, form: &[(&str, String)]) -> Result<String> {
        for attempt in 0..2 {
            let resp = self
                .http
                .post(self.url(path))
                .header("Referer", self.referer())
                .form(form)
                .send()
                .await?;
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                if attempt == 0 {
                    self.login().await?;
                    continue;
                }
                return Err(AppError::QbitAuth);
            }
            let ok = resp.status().is_success();
            let body = resp.text().await.unwrap_or_default();
            if !ok {
                return Err(AppError::Qbit { status, body: body.chars().take(300).collect() });
            }
            return Ok(body);
        }
        unreachable!()
    }

    /// Raw maindata (server_state carries connection/DHT health).
    pub async fn sync_maindata(&self) -> Result<serde_json::Value> {
        Ok(self
            .get_authed("/api/v2/sync/maindata", &[("rid", "0".into())])
            .await?
            .json()
            .await?)
    }

    pub async fn version(&self) -> Result<String> {
        Ok(self.get_authed("/api/v2/app/version", &[]).await?.text().await?)
    }

    pub async fn ensure_category(&self, name: &str, save_path: &str) -> Result<()> {
        let body = self
            .post_authed(
                "/api/v2/torrents/createCategory",
                &[("category", name.to_string()), ("savePath", save_path.to_string())],
            )
            .await;
        match body {
            Ok(_) => Ok(()),
            // 409 = category already exists; fine.
            Err(AppError::Qbit { status: 409, .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn add(&self, t: AddTorrent<'_>) -> Result<()> {
        // Login preflight: multipart Form is not Clone, so we cannot cheaply
        // retry-after-401 like the form endpoints do.
        let _ = self.version().await;

        let mut form = reqwest::multipart::Form::new();
        if let Some(magnet) = &t.magnet {
            form = form.text("urls", magnet.clone());
        } else if let Some(bytes) = t.torrent_bytes {
            let part = reqwest::multipart::Part::bytes(bytes)
                .file_name(format!("{}.torrent", sanitize(t.torrent_name)))
                .mime_str("application/x-bittorrent")
                .map_err(|e| AppError::Other(e.to_string()))?;
            form = form.part("torrents", part);
        } else {
            return Err(AppError::NoDownloadSource);
        }
        if let Some(p) = &t.save_path {
            if !p.is_empty() {
                form = form.text("savepath", p.clone());
            }
        }
        if let Some(c) = &t.category {
            if !c.is_empty() {
                form = form.text("category", c.clone());
            }
        }
        // v5 renamed "paused" to "stopped"; send both for compatibility.
        let flag = if t.paused { "true" } else { "false" };
        form = form.text("paused", flag).text("stopped", flag);
        if let Some(ratio) = t.ratio_limit {
            form = form.text("ratioLimit", format!("{ratio}"));
            if ratio <= 0.0 {
                // stop-when-complete: also cap seeding time so a 0-ratio swarm
                // cannot idle forever in "seeding" state
                form = form.text("seedingTimeLimit", "0");
            }
        }

        let resp = self
            .http
            .post(self.url("/api/v2/torrents/add"))
            .header("Referer", self.referer())
            .multipart(form)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let ok = resp.status().is_success();
        let body = resp.text().await.unwrap_or_default();
        if status == 401 || status == 403 {
            return Err(AppError::QbitAuth);
        }
        if !ok {
            return Err(AppError::Qbit { status, body: body.chars().take(300).collect() });
        }
        if body.contains("Fails") {
            return Err(AppError::Other(
                "qBittorrent rejected the torrent (duplicate or invalid)".into(),
            ));
        }
        Ok(())
    }

    pub async fn list(&self, category: Option<&str>) -> Result<Vec<QbitTorrent>> {
        let mut q: Vec<(&str, String)> =
            vec![("sort", "added_on".into()), ("reverse", "true".into())];
        if let Some(c) = category {
            q.push(("category", c.to_string()));
        }
        Ok(self.get_authed("/api/v2/torrents/info", &q).await?.json().await?)
    }

    /// Free bytes on the download disk, from qBittorrent's own accounting.
    pub async fn free_space(&self) -> Result<i64> {
        let resp = self
            .get_authed("/api/v2/sync/maindata", &[("rid", "0".into())])
            .await?;
        let v: serde_json::Value = resp.json().await?;
        v.get("server_state")
            .and_then(|s| s.get("free_space_on_disk"))
            .and_then(|f| f.as_i64())
            .ok_or_else(|| AppError::Other("qBittorrent did not report free disk space".into()))
    }

    pub async fn transfer_info(&self) -> Result<TransferInfo> {
        Ok(self.get_authed("/api/v2/transfer/info", &[]).await?.json().await?)
    }

    /// action: "stop" | "start" | "delete" | "deleteWithFiles"
    pub async fn torrent_action(&self, action: &str, hash: &str) -> Result<()> {
        match action {
            "stop" | "start" => {
                // v5 renamed pause/resume -> stop/start; on 4.x the new names
                // 404, so fall back to the legacy ones
                let legacy = if action == "stop" { "pause" } else { "resume" };
                let r = self
                    .post_authed(
                        &format!("/api/v2/torrents/{action}"),
                        &[("hashes", hash.to_string())],
                    )
                    .await;
                if let Err(AppError::Qbit { status: 404, .. }) = &r {
                    self.post_authed(
                        &format!("/api/v2/torrents/{legacy}"),
                        &[("hashes", hash.to_string())],
                    )
                    .await?;
                } else {
                    r?;
                }
            }
            "delete" | "deleteWithFiles" => {
                let del_files = (action == "deleteWithFiles").to_string();
                self.post_authed(
                    "/api/v2/torrents/delete",
                    &[("hashes", hash.to_string()), ("deleteFiles", del_files)],
                )
                .await?;
            }
            _ => return Err(AppError::Other(format!("unknown action {action}"))),
        }
        Ok(())
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || " .-_[]()".contains(c) { c } else { '_' })
        .take(120)
        .collect()
}
