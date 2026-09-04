use serde::{Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("network error: {0}")]
    Http(reqwest::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("bad json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0} is not configured yet — open Settings")]
    NotConfigured(&'static str),

    /// SQLite failures keep their code: startup must tell a corrupt file
    /// (move it aside, start fresh) from a busy or locked one (never touch it).
    #[error("database error: {message}")]
    Db { code: Option<rusqlite::ErrorCode>, message: String },

    #[error("Prowlarr rejected the request ({status}): {body}")]
    Prowlarr { status: u16, body: String },

    #[error("qBittorrent rejected the request ({status}): {body}")]
    Qbit { status: u16, body: String },

    #[error("qBittorrent login failed — check the username and password")]
    QbitAuth,

    /// qBittorrent answered "Fails." to an add: the torrent is already in the
    /// session. Not a failure of the grab — the content is there — so the
    /// dispatcher adopts the existing torrent instead of abandoning the claim
    /// and retrying the same release every cycle.
    #[error("qBittorrent already has this torrent")]
    QbitDuplicate,

    #[error("this release has no usable magnet link or download URL")]
    NoDownloadSource,

    /// The backend request may have been accepted, but its response was lost.
    /// The durable dispatch row must remain until backend reconciliation.
    #[error("{0}")]
    DispatchUncertain(String),

    #[error("{0}")]
    Other(String),
}

// reqwest's Display appends "for url (…)" - and our URLs carry Prowlarr's API
// key in the query and tracker passkeys in the path. Strip it at the boundary
// so no toast, activity row, or agent tool result can ever repeat a secret.
impl From<reqwest::Error> for AppError {
    fn from(error: reqwest::Error) -> Self {
        AppError::Http(error.without_url())
    }
}

// Tauri commands must return something serializable; collapse to a message.
impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

pub type Result<T> = std::result::Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::AppError;

    #[tokio::test]
    async fn transport_errors_never_carry_the_request_url() {
        // Prowlarr download links carry the API key in the query and tracker
        // passkeys in the path; a connect failure must not echo them into
        // toasts, the activity feed, or the agent's tool results
        let http = reqwest::Client::new();
        let error = http
            .get("http://127.0.0.1:1/api/v1/download?apikey=SECRETKEY123&link=x")
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .unwrap_err();
        let app: AppError = error.into();
        let text = app.to_string();
        assert!(!text.contains("SECRETKEY123"), "{text}");
        assert!(!text.contains("127.0.0.1:1/"), "{text}");
        assert!(text.starts_with("network error:"), "{text}");
    }
}
