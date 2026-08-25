use serde::{Deserialize, Serialize};
use crate::error::{AppError, Result};

const MAX_TORRENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_DOWNLOAD_REDIRECTS: usize = 5;

fn same_origin(left: &url::Url, right: &url::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProwlarrRelease {
    pub guid: Option<String>,
    pub title: String,
    #[serde(default)]
    pub size: i64,
    pub indexer: Option<String>,
    #[serde(default)]
    pub indexer_id: i32,
    pub info_url: Option<String>,
    pub download_url: Option<String>,
    pub magnet_url: Option<String>,
    pub info_hash: Option<String>,
    pub seeders: Option<i32>,
    pub leechers: Option<i32>,
    pub protocol: Option<String>,
    pub publish_date: Option<String>,
    #[serde(default)]
    pub age: i64,
    #[serde(default)]
    pub grabs: Option<i32>,
    #[serde(default)]
    pub categories: Vec<ProwlarrCategory>,
    pub imdb_id: Option<serde_json::Value>,
    pub tmdb_id: Option<i64>,
    pub tvdb_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProwlarrCategory {
    pub id: i32,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProwlarrIndexer {
    pub id: i32,
    pub name: String,
    #[serde(default)]
    pub enable: bool,
    pub protocol: Option<String>,
    #[serde(default)]
    pub privacy: Option<String>,
    #[serde(default)]
    pub capabilities: Option<serde_json::Value>,
}

pub struct ProwlarrClient<'a> {
    pub http: &'a reqwest::Client,
    pub base: String,
    pub api_key: String,
}

impl<'a> ProwlarrClient<'a> {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(AppError::Prowlarr {
            status: status.as_u16(),
            body: body.chars().take(300).collect(),
        })
    }

    pub async fn ping(&self) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(self.url("/api/v1/system/status"))
            .header("X-Api-Key", &self.api_key)
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn indexers(&self) -> Result<Vec<ProwlarrIndexer>> {
        let resp = self
            .http
            .get(self.url("/api/v1/indexer"))
            .header("X-Api-Key", &self.api_key)
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    /// categories: 2000 movies, 5000 tv. Empty indexer_ids = all enabled indexers.
    pub async fn search(
        &self,
        query: &str,
        categories: &[i32],
        indexer_ids: &[i32],
        limit: i32,
    ) -> Result<Vec<ProwlarrRelease>> {
        let mut req = self
            .http
            .get(self.url("/api/v1/search"))
            .header("X-Api-Key", &self.api_key)
            .query(&[("query", query), ("type", "search")])
            .query(&[("limit", limit.to_string().as_str()), ("offset", "0")]);
        for c in categories {
            req = req.query(&[("categories", c.to_string())]);
        }
        for i in indexer_ids {
            req = req.query(&[("indexerIds", i.to_string())]);
        }
        let resp = req
            // the shared client's 60s ceiling is right for API calls, but a
            // search fans out to every enabled indexer — slow ones routinely
            // push the aggregate past it while Prowlarr still returns
            // partial results worth having
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    /// All installable indexer definitions (raw JSON — large; slim before returning to UI).
    pub async fn schema(&self) -> Result<Vec<serde_json::Value>> {
        let resp = self
            .http
            .get(self.url("/api/v1/indexer/schema"))
            .header("X-Api-Key", &self.api_key)
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn get_indexer_raw(&self, id: i32) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(self.url(&format!("/api/v1/indexer/{id}")))
            .header("X-Api-Key", &self.api_key)
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn add_indexer_raw(&self, def: &serde_json::Value) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(self.url("/api/v1/indexer"))
            .header("X-Api-Key", &self.api_key)
            .json(def)
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn update_indexer_raw(&self, id: i32, def: &serde_json::Value) -> Result<()> {
        let resp = self
            .http
            .put(self.url(&format!("/api/v1/indexer/{id}")))
            .header("X-Api-Key", &self.api_key)
            .json(def)
            .send()
            .await?;
        Self::check(resp).await?;
        Ok(())
    }

    pub async fn delete_indexer(&self, id: i32) -> Result<()> {
        let resp = self
            .http
            .delete(self.url(&format!("/api/v1/indexer/{id}")))
            .header("X-Api-Key", &self.api_key)
            .send()
            .await?;
        Self::check(resp).await?;
        Ok(())
    }

    /// All tags known to Prowlarr.
    pub async fn tags(&self) -> Result<Vec<serde_json::Value>> {
        let resp = self
            .http
            .get(format!("{}/api/v1/tag", self.base))
            .header("X-Api-Key", &self.api_key)
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    /// Find a tag by label or create it; returns its id.
    pub async fn ensure_tag(&self, label: &str) -> Result<i64> {
        let tags = self.tags().await?;
        if let Some(id) = tags
            .iter()
            .find(|t| t.get("label").and_then(|v| v.as_str()) == Some(label))
            .and_then(|t| t.get("id").and_then(|v| v.as_i64()))
        {
            return Ok(id);
        }
        let resp = self
            .http
            .post(format!("{}/api/v1/tag", self.base))
            .header("X-Api-Key", &self.api_key)
            .json(&serde_json::json!({ "label": label }))
            .send()
            .await?;
        let created: serde_json::Value = Self::check(resp).await?.json().await?;
        created
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| AppError::Other("Prowlarr returned a tag without an id".into()))
    }

    /// Indexer proxies configured in Prowlarr.
    pub async fn indexer_proxies(&self) -> Result<Vec<serde_json::Value>> {
        let resp = self
            .http
            .get(format!("{}/api/v1/indexerproxy", self.base))
            .header("X-Api-Key", &self.api_key)
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    /// Is any FlareSolverr proxy registered (ours or the user's own)?
    pub async fn flaresolverr_proxy_exists(&self) -> Result<bool> {
        Ok(self
            .indexer_proxies()
            .await?
            .iter()
            .any(|p| p.get("implementation").and_then(|v| v.as_str()) == Some("FlareSolverr")))
    }

    /// Register FlareSolverr as an indexer proxy, applied to indexers carrying
    /// tag_id. If the user already has their own FlareSolverr proxy, merge our
    /// tag into it instead of leaving a tag that matches nothing.
    pub async fn ensure_flaresolverr_proxy(&self, tag_id: i64) -> Result<()> {
        let existing = self.indexer_proxies().await?;
        if let Some(p) = existing
            .iter()
            .find(|p| p.get("implementation").and_then(|v| v.as_str()) == Some("FlareSolverr"))
        {
            let mut p = p.clone();
            let mut tags: Vec<i64> = p
                .get("tags")
                .and_then(|t| t.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
                .unwrap_or_default();
            if tags.contains(&tag_id) {
                return Ok(());
            }
            tags.push(tag_id);
            p["tags"] = serde_json::json!(tags);
            let id = p.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            let resp = self
                .http
                .put(format!("{}/api/v1/indexerproxy/{id}", self.base))
                .header("X-Api-Key", &self.api_key)
                .json(&p)
                .send()
                .await?;
            Self::check(resp).await?;
            return Ok(());
        }
        let resp = self
            .http
            .get(format!("{}/api/v1/indexerproxy/schema", self.base))
            .header("X-Api-Key", &self.api_key)
            .send()
            .await?;
        let schema: Vec<serde_json::Value> = Self::check(resp).await?.json().await?;
        let mut def = schema
            .into_iter()
            .find(|d| d.get("implementation").and_then(|v| v.as_str()) == Some("FlareSolverr"))
            .ok_or_else(|| AppError::Other("this Prowlarr doesn't offer a FlareSolverr proxy".into()))?;
        def["name"] = serde_json::Value::from("FlareSolverr (Trawler)");
        def["tags"] = serde_json::json!([tag_id]);
        // pin the host: FlareSolverr binds IPv4, and "localhost" can resolve
        // to ::1 first on some machines — don't trust the schema default
        if let Some(fields) = def.get_mut("fields").and_then(|f| f.as_array_mut()) {
            for f in fields {
                match f.get("name").and_then(|n| n.as_str()) {
                    Some("host") => f["value"] = serde_json::Value::from("http://127.0.0.1:8191/"),
                    Some("requestTimeout") => f["value"] = serde_json::Value::from(60),
                    _ => {}
                }
            }
        }
        let resp = self
            .http
            .post(format!("{}/api/v1/indexerproxy", self.base))
            .header("X-Api-Key", &self.api_key)
            .json(&def)
            .send()
            .await?;
        Self::check(resp)
            .await
            .map_err(|e| AppError::Other(format!("couldn't register FlareSolverr in Prowlarr: {e}")))?;
        Ok(())
    }

    /// Undo the unlock: delete our proxy and the flaresolverr tag (Prowlarr
    /// clears the tag off indexers itself). The user's own proxies are kept.
    pub async fn remove_flaresolverr(&self) -> Result<()> {
        for p in self.indexer_proxies().await? {
            if p.get("implementation").and_then(|v| v.as_str()) == Some("FlareSolverr")
                && p.get("name").and_then(|v| v.as_str()) == Some("FlareSolverr (Trawler)")
            {
                if let Some(id) = p.get("id").and_then(|v| v.as_i64()) {
                    let resp = self
                        .http
                        .delete(format!("{}/api/v1/indexerproxy/{id}", self.base))
                        .header("X-Api-Key", &self.api_key)
                        .send()
                        .await?;
                    Self::check(resp).await?;
                }
            }
        }
        for t in self.tags().await? {
            if t.get("label").and_then(|v| v.as_str()) == Some("flaresolverr") {
                if let Some(id) = t.get("id").and_then(|v| v.as_i64()) {
                    let resp = self
                        .http
                        .delete(format!("{}/api/v1/tag/{id}", self.base))
                        .header("X-Api-Key", &self.api_key)
                        .send()
                        .await?;
                    Self::check(resp).await?;
                }
            }
        }
        Ok(())
    }

    /// Fetch .torrent bytes through Prowlarr's proxy (keeps passkeys server-side).
    pub async fn fetch_torrent(&self, download_url: &str) -> Result<(Vec<u8>, Option<String>)> {
        let base = url::Url::parse(&self.base)
            .map_err(|e| AppError::Other(format!("invalid Prowlarr URL: {e}")))?;
        let mut current = url::Url::parse(download_url)
            .map_err(|e| AppError::Other(format!("invalid indexer download URL: {e}")))?;
        if current.scheme() == "magnet" {
            return Ok((Vec::new(), Some(current.to_string())));
        }
        if !matches!(current.scheme(), "http" | "https") {
            return Err(AppError::Other(
                "indexer download URL must use HTTP, HTTPS, or magnet".into(),
            ));
        }

        // The shared client follows redirects automatically, which would also
        // forward our custom X-Api-Key header. Follow manually so the key is
        // attached only to Prowlarr's exact origin and never to an indexer.
        let download_client = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("trawler/{}", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(60))
            .build()?;

        let mut response = None;
        for redirects in 0..=MAX_DOWNLOAD_REDIRECTS {
            let mut request = download_client.get(current.clone());
            if same_origin(&current, &base) {
                request = request.header("X-Api-Key", &self.api_key);
            }
            let resp = request.send().await?;
            if !resp.status().is_redirection() {
                response = Some(Self::check(resp).await?);
                break;
            }
            if redirects == MAX_DOWNLOAD_REDIRECTS {
                return Err(AppError::Other(format!(
                    "indexer download exceeded {MAX_DOWNLOAD_REDIRECTS} redirects"
                )));
            }
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| AppError::Other("indexer returned a redirect without a valid Location".into()))?;
            let next = current
                .join(location)
                .or_else(|_| url::Url::parse(location))
                .map_err(|e| AppError::Other(format!("invalid indexer redirect: {e}")))?;
            if next.scheme() == "magnet" {
                return Ok((Vec::new(), Some(next.to_string())));
            }
            if !matches!(next.scheme(), "http" | "https") {
                return Err(AppError::Other(format!(
                    "indexer redirected to unsupported scheme {}",
                    next.scheme()
                )));
            }
            current = next;
        }
        let mut resp = response.ok_or_else(|| AppError::Other("indexer download did not return a response".into()))?;
        if resp.content_length().is_some_and(|n| n > MAX_TORRENT_BYTES as u64) {
            return Err(AppError::Other(format!(
                "torrent file is larger than the {} MB safety limit",
                MAX_TORRENT_BYTES / 1024 / 1024
            )));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > MAX_TORRENT_BYTES {
                return Err(AppError::Other(format!(
                    "torrent file is larger than the {} MB safety limit",
                    MAX_TORRENT_BYTES / 1024 / 1024
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.starts_with(b"magnet:") {
            let m = String::from_utf8_lossy(&bytes).trim().to_string();
            return Ok((Vec::new(), Some(m)));
        }
        // .torrent files are bencoded dicts and always start with 'd'
        if !bytes.starts_with(b"d") {
            return Err(AppError::Other(format!(
                "indexer returned something that is not a torrent file ({} bytes, starts with {:?})",
                bytes.len(),
                String::from_utf8_lossy(&bytes[..bytes.len().min(40)])
            )));
        }
        Ok((bytes, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn one_shot_server(response: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 8192];
            let count = socket.read(&mut request).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request[..count]).to_string()
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn redirect_does_not_leak_prowlarr_api_key() {
        let body = "d3:foo3:bare";
        let external_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (external, external_task) = one_shot_server(external_response).await;
        let prowlarr_response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {external}/file.torrent\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let (prowlarr, prowlarr_task) = one_shot_server(prowlarr_response).await;
        let http = reqwest::Client::builder().build().unwrap();
        let client = ProwlarrClient {
            http: &http,
            base: prowlarr.clone(),
            api_key: "top-secret".into(),
        };

        let (bytes, magnet) = client
            .fetch_torrent(&format!("{prowlarr}/download"))
            .await
            .unwrap();
        assert_eq!(bytes, body.as_bytes());
        assert!(magnet.is_none());
        assert!(
            prowlarr_task
                .await
                .unwrap()
                .to_ascii_lowercase()
                .contains("x-api-key: top-secret")
        );
        assert!(!external_task.await.unwrap().contains("top-secret"));
    }

    #[tokio::test]
    async fn oversized_torrent_is_rejected_before_reading_body() {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_TORRENT_BYTES + 1
        );
        let (server, task) = one_shot_server(response).await;
        let http = reqwest::Client::builder().build().unwrap();
        let client = ProwlarrClient {
            http: &http,
            base: server.clone(),
            api_key: "key".into(),
        };
        let error = client
            .fetch_torrent(&format!("{server}/download"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("safety limit"));
        let _ = task.await.unwrap();
    }
}
