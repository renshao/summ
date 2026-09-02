use thiserror::Error;

#[derive(Debug, Error)]
pub enum SummError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("not found")]
    NotFound,
    #[error("invalid data: {0}")]
    InvalidData(String),
    #[error("invalid digest: {0}")]
    InvalidDigest(String),
}

pub type Result<T> = std::result::Result<T, SummError>;
