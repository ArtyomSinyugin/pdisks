#![forbid(unsafe_code)]

//! Foreground command adapter over the application facade; saved files grant no authorization.

pub mod backend4;
mod commands;
pub mod confirmation;
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
    Usage(#[serde(serialize_with = "disksgraph::diagnostics::serialize_message")] String),
    /// A document could not be read, published or synchronized.
    #[error("{0}")]
    Io(#[serde(serialize_with = "disksgraph::diagnostics::serialize_message")] String),
    /// Input exceeded the bounded file contract or failed JSON validation.
    #[error("{0}")]
    Document(#[serde(serialize_with = "disksgraph::diagnostics::serialize_message")] String),
    /// The shared application rejected the operation.
    #[error(transparent)]
    Application(#[from] disksgraph::DisksGraphError),
    /// Explicit native host configuration could not be loaded.
    #[error("{0}")]
    Configuration(#[serde(serialize_with = "disksgraph::diagnostics::serialize_message")] String),
}

impl Error {
    /// Stable process category: malformed input is 2, host/application/output failure is 1.
    /// Use the application code rather than message text for typed saved-document failures.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) | Self::Document(_) => 2,
            Self::Application(error)
                if matches!(
                    error.code,
                    disksgraph::ErrorCode::InvalidDraft | disksgraph::ErrorCode::InvalidPlan
                ) =>
            {
                2
            }
            _ => 1,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Embedders and the executable classify typed validation errors without parsing diagnostics.
    #[test]
    fn error_exit_codes_distinguish_input_from_host_failures() {
        for error in [Error::Usage("usage".into()), Error::Document("json".into())] {
            assert_eq!(error.exit_code(), 2);
        }
        for error in [
            Error::Io("output".into()),
            Error::Configuration("manifest".into()),
        ] {
            assert_eq!(error.exit_code(), 1);
        }
        for (code, expected) in [
            (disksgraph::ErrorCode::InvalidDraft, 2),
            (disksgraph::ErrorCode::InvalidPlan, 2),
            (disksgraph::ErrorCode::LeaseUnavailable, 1),
            (disksgraph::ErrorCode::CheckpointBlocked, 1),
            (disksgraph::ErrorCode::VerificationFailed, 1),
        ] {
            assert_eq!(
                Error::Application(disksgraph::DisksGraphError {
                    code,
                    message: "same opaque diagnostic".into(),
                    run_id: None,
                })
                .exit_code(),
                expected
            );
        }
    }
}
