use std::path::PathBuf;

use crate::Engine;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("the {0} engine is not available: it is either not supported on this platform or its Cargo feature is disabled")]
    EngineUnavailable(Engine),

    #[error(
        "no web engine is available in this build; enable one of polybrowser's engine features"
    )]
    NoEngine,

    #[error("unknown engine `{0}`")]
    UnknownEngine(String),

    #[error("{engine} backend: {message}")]
    Backend { engine: Engine, message: String },

    #[error("the window handle is of a kind the {0} engine cannot embed into")]
    UnsupportedWindowHandle(Engine),

    #[error(transparent)]
    WindowHandle(#[from] raw_window_handle::HandleError),

    #[error("browser views can only be used from the main thread")]
    NotMainThread,

    #[error("no profile named `{0}`")]
    ProfileNotFound(String),

    #[error("a profile named `{0}` already exists")]
    ProfileExists(String),

    #[error("invalid profile name: {0}")]
    InvalidProfileName(&'static str),

    #[error("could not determine a data directory for this platform")]
    NoDataDirectory,

    #[error("no browser data found at {0}")]
    SourceNotFound(PathBuf),

    #[error("not allowed to read {path}: {hint}")]
    PermissionDenied { path: PathBuf, hint: &'static str },

    #[error("{0} data cannot be read on this platform")]
    UnsupportedSource(&'static str),

    #[error("unreadable {what}: {message}")]
    Malformed { what: &'static str, message: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Database(#[from] rusqlite::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Shorthand used by engine backends to report a failure of the engine itself.
    pub fn backend(engine: Engine, message: impl std::fmt::Display) -> Self {
        Error::Backend {
            engine,
            message: message.to_string(),
        }
    }
}
