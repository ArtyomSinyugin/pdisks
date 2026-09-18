use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Command failed: {program}: {details}")]
    CommandFailed { program: String, details: String },

    #[error("Missing required output from fixture file")]
    MissingOutput,
}

pub type Result<T> = std::result::Result<T, ProbeError>;
