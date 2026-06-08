use thiserror::Error;

#[derive(Error, Debug)]
pub enum ShredError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Permission denied: {0}")]
    Permission(String),

    #[error("File not found: {0}")]
    NotFound(String),

    #[error("Operation cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}
