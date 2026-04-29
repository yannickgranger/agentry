//! Error type for the runtime.

use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("podman: {0}")]
    Podman(String),
    #[error("failed to load role from {path}: {source}")]
    RoleLoadFailed {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("not found: {kind} {key}")]
    NotFound { kind: &'static str, key: String },
    #[error("config: {0}")]
    Config(String),
    #[error("spawn: {0}")]
    Spawn(String),
    #[error("agent exited without done event")]
    NoTerminalEvent,
}

pub type Result<T> = std::result::Result<T, Error>;
