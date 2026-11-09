use thiserror::Error;

#[derive(Debug, Error)]
pub enum ShoalError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("corrupt data: {0}")]
    Corrupt(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl ShoalError {
    /// HTTP status code mapping used by the API server.
    pub fn http_status(&self) -> u16 {
        match self {
            ShoalError::NotFound(_) => 404,
            ShoalError::AlreadyExists(_) | ShoalError::Conflict(_) => 409,
            ShoalError::InvalidRequest(_) => 400,
            _ => 500,
        }
    }
}

impl From<serde_json::Error> for ShoalError {
    fn from(e: serde_json::Error) -> Self {
        ShoalError::Serde(e.to_string())
    }
}

impl From<object_store::Error> for ShoalError {
    fn from(e: object_store::Error) -> Self {
        match e {
            object_store::Error::NotFound { path, .. } => ShoalError::NotFound(path),
            object_store::Error::AlreadyExists { path, .. } => ShoalError::AlreadyExists(path),
            other => ShoalError::Storage(other.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, ShoalError>;
