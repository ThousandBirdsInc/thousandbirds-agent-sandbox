use std::fmt;
use std::io;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    Io {
        path: Option<PathBuf>,
        source: io::Error,
    },
    InvalidArgument(String),
    InvalidImage(String),
    InvalidCredential(String),
    NotFound(String),
    ProcessFailed {
        command: String,
        status: std::process::ExitStatus,
    },
    Libkrun {
        operation: &'static str,
        code: i32,
    },
    /// Admission control refused to start a sandbox because doing so would
    /// exceed the configured host memory budget.
    ResourceExhausted(String),
    /// An internal failure that is not the caller's fault, e.g. a background
    /// task panicked or could not be joined.
    Internal(String),
}

impl Error {
    pub fn io_path(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: Some(path.into()),
            source,
        }
    }

    pub fn io(source: io::Error) -> Self {
        Self::Io { path: None, source }
    }
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Self::io(source)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io {
                path: Some(path),
                source,
            } => write!(f, "{}: {}", path.display(), source),
            Error::Io { path: None, source } => write!(f, "{source}"),
            Error::InvalidArgument(message) => write!(f, "invalid argument: {message}"),
            Error::InvalidImage(message) => write!(f, "invalid image: {message}"),
            Error::InvalidCredential(message) => write!(f, "invalid credential: {message}"),
            Error::NotFound(message) => write!(f, "not found: {message}"),
            Error::ProcessFailed { command, status } => {
                write!(f, "{command} exited with status {status}")
            }
            Error::Libkrun { operation, code } => {
                write!(f, "{operation} failed with libkrun error {code}")
            }
            Error::ResourceExhausted(message) => write!(f, "resource exhausted: {message}"),
            Error::Internal(message) => write!(f, "internal error: {message}"),
        }
    }
}

impl std::error::Error for Error {}
