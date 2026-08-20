use serde::{Deserialize, Serialize};
use crate::error::{AppError, Result};

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
        let resp = req.send().await?;
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

    /// Fetch .torrent bytes through Prowlarr's proxy (keeps passkeys server-side).
    pub async fn fetch_torrent(&self, download_url: &str) -> Result<(Vec<u8>, Option<String>)> {
        // the download_url comes from indexer data — only hand Prowlarr's API
        // key to Prowlarr itself, never to an arbitrary host it points at
        let same_host = url::Url::parse(download_url)
            .ok()
            .zip(url::Url::parse(&self.base).ok())
            .map(|(u, b)| u.host_str() == b.host_str() && u.port_or_known_default() == b.port_or_known_default())
            .unwrap_or(false);
        // a magnet in the download link is a redirect target — capture it
        if download_url.starts_with("magnet:") {
            return Ok((Vec::new(), Some(download_url.to_string())));
        }
        let mut req = self.http.get(download_url);
        if same_host {
            req = req.header("X-Api-Key", &self.api_key);
        }
        let resp = req.send().await?;
        let resp = Self::check(resp).await?;
        // A "torrent" download link can still bounce to a magnet redirect.
        let final_url = resp.url().to_string();
        if final_url.starts_with("magnet:") {
            return Ok((Vec::new(), Some(final_url)));
        }
        let name = resp
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split("filename=").nth(1))
            .map(|v| v.trim_matches(['"', '\'', ';', ' ']).to_string());
        let bytes = resp.bytes().await?.to_vec();
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
        let _ = name;
        Ok((bytes, None))
    }
}
