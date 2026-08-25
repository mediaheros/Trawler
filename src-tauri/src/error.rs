use serde::{Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("bad json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0} is not configured yet — open Settings")]
    NotConfigured(&'static str),

    #[error("Prowlarr rejected the request ({status}): {body}")]
    Prowlarr { status: u16, body: String },

    #[error("qBittorrent rejected the request ({status}): {body}")]
    Qbit { status: u16, body: String },

    #[error("qBittorrent login failed — check the username and password")]
    QbitAuth,

    #[error("this release has no usable magnet link or download URL")]
    NoDownloadSource,

    /// The backend request may have been accepted, but its response was lost.
    /// The durable dispatch row must remain until backend reconciliation.
    #[error("{0}")]
    DispatchUncertain(String),

    #[error("{0}")]
    Other(String),
}

// Tauri commands must return something serializable; collapse to a message.
impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
