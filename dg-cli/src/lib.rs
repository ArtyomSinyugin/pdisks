#![forbid(unsafe_code)]

//! Foreground command adapter over the application facade; saved files grant no authorization.

mod commands;
pub mod files;
pub mod incident;
pub mod repl;
pub use commands::{Command, Invocation, execute, execute_with_io};

/// Completed commands succeed only after actual final verification; event I/O has its own failure.
/// Hosts embedding the command adapter use the exact executable's result/exit convention.
pub fn result_exit_code(value: &serde_json::Value) -> u8 {
    if !value["event_output_error"].is_null() {
        1
    } else if value.get("run").is_some() && value["run"]["verified"] != true {
        3
    } else {
        0
    }
}

/// Structured command failures; application diagnostics retain their original stable code.
#[derive(Debug, serde::Serialize, thiserror::Error)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum Error {
    /// Invalid command or unsupported option, rejected before configuring native providers.
    #[error("{0}")]
    Usage(String),
    /// A document could not be read, published or synchronized.
    #[error("{0}")]
    Io(String),
    /// Input exceeded the bounded file contract or failed JSON validation.
    #[error("{0}")]
    Document(String),
    /// The shared application rejected the operation.
    #[error(transparent)]
    Application(#[from] disksgraph::DisksGraphError),
    /// Explicit native host configuration could not be loaded.
    #[error("{0}")]
    Configuration(String),
}

impl From<std::io::Error> for Error {
    /// Keeps OS failures separate from parse and application errors.
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<serde_json::Error> for Error {
    /// Encoding I/O failures retain the same exit category as other broken output streams.
    fn from(error: serde_json::Error) -> Self {
        if error.is_io() {
            Self::Io(error.to_string())
        } else {
            Self::Document(error.to_string())
        }
    }
}
