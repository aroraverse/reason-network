use thiserror::Error;

/// Errors surfaced by every layer of the system.
///
/// The daemon converts these into protocol level error responses so that
/// clients always receive a machine readable code plus a human message.
#[derive(Debug, Error)]
pub enum Error {
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("ipc error: {0}")]
    Ipc(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("daemon is not running: {0}")]
    DaemonUnavailable(String),

    #[error("delivery error: {0}")]
    Delivery(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Stable, machine readable code used in protocol responses.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Storage(_) => "storage_error",
            Error::Io(_) => "io_error",
            Error::Serde(_) => "serialization_error",
            Error::Ipc(_) => "ipc_error",
            Error::NotFound(_) => "not_found",
            Error::InvalidRequest(_) => "invalid_request",
            Error::DaemonUnavailable(_) => "daemon_unavailable",
            Error::Delivery(_) => "delivery_error",
            Error::Other(_) => "error",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
