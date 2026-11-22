use thiserror::Error;

#[derive(Debug, Error)]
pub enum OctoError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Anti-loop guard rejected request: {0}")]
    LoopGuard(String),

    #[error("Unknown target peer or role: '{0}'")]
    UnknownTarget(String),

    #[error("All candidate peers failed or are on cooldown for target '{target}': {details}")]
    AllPeersExhausted { target: String, details: String },

    #[error("Adapter execution failed: {0}")]
    AdapterFailure(String),
}

pub type Result<T> = std::result::Result<T, OctoError>;
