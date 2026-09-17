use std::io;
use std::path::PathBuf;

/// Recoverable failure for a whole pack run.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Message(String),
    #[error("{path}: {message}")]
    Path { path: PathBuf, message: String },
}

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    pub fn path(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Path {
            path: path.into(),
            message: message.into(),
        }
    }
}
