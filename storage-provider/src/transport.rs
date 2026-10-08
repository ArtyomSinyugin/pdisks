//! Request delivery boundary for built-in and external logical providers.
//!
//! This protocol belongs to PDisks. It is deliberately independent from the C
//! ABI used by system backends such as libcryptsetup.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ProviderId;

/// Current serialized PDisks provider protocol version.
pub const PROVIDER_PROTOCOL_VERSION: u16 = 1;

/// Coarse provider attachment mechanism used in diagnostics and policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderConnectionKind {
    /// Provider implementation linked into the host process.
    BuiltIn,
    /// Separately built provider loaded through the future PDisks Stabby ABI.
    StabbyDynamicLibrary,
    /// Provider served by a separately supervised process.
    Process,
}

/// Configuration locating one logical provider implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderConnection {
    /// Provider implementation linked into the host process.
    BuiltIn,
    /// PDisks provider plugin loaded through the project-owned Stabby ABI.
    StabbyDynamicLibrary {
        /// Absolute path to the PDisks provider plugin, not to a system C library.
        library: PathBuf,
        /// Versioned PDisks plugin entrypoint exported through Stabby.
        entrypoint: String,
    },
    /// Provider reached through a separately supervised process.
    Process {
        /// Absolute path to the provider executable.
        executable: PathBuf,
    },
}

impl ProviderConnection {
    /// Returns the attachment category without exposing configuration details.
    pub const fn kind(&self) -> ProviderConnectionKind {
        match self {
            Self::BuiltIn => ProviderConnectionKind::BuiltIn,
            Self::StabbyDynamicLibrary { .. } => ProviderConnectionKind::StabbyDynamicLibrary,
            Self::Process { .. } => ProviderConnectionKind::Process,
        }
    }
}

/// Binds a logical provider identity to one delivery mechanism.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderBinding {
    /// Logical technology provider receiving requests.
    pub provider: ProviderId,
    /// Mechanism used to deliver PDisks requests and responses.
    pub connection: ProviderConnection,
}

/// Transport-neutral request envelope owned and versioned by PDisks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequest {
    /// Provider protocol schema used by this envelope.
    pub protocol_version: u16,
    /// Caller-generated request identity used to correlate the response.
    pub request_id: u64,
    /// Logical provider expected to interpret the payload.
    pub provider: ProviderId,
    /// Provider-specific serialized request containing no secret material.
    pub payload: Vec<u8>,
}

impl ProviderRequest {
    /// Creates a request using the current provider protocol version.
    pub fn new(request_id: u64, provider: ProviderId, payload: Vec<u8>) -> Self {
        Self {
            protocol_version: PROVIDER_PROTOCOL_VERSION,
            request_id,
            provider,
            payload,
        }
    }

    /// Rejects an envelope from an unsupported provider protocol version.
    pub fn validate(&self) -> Result<(), ProviderTransportError> {
        if self.protocol_version == PROVIDER_PROTOCOL_VERSION {
            Ok(())
        } else {
            Err(ProviderTransportError::UnsupportedProtocol {
                version: self.protocol_version,
            })
        }
    }
}

/// Transport-neutral provider response correlated to one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderResponse {
    /// Provider protocol schema used by this envelope.
    pub protocol_version: u16,
    /// Identity copied from the matching request.
    pub request_id: u64,
    /// Successful provider bytes or a structured remote error.
    pub payload: ProviderResponsePayload,
}

impl ProviderResponse {
    /// Builds a successful response for one request.
    pub fn success(request: &ProviderRequest, payload: Vec<u8>) -> Self {
        Self {
            protocol_version: PROVIDER_PROTOCOL_VERSION,
            request_id: request.request_id,
            payload: ProviderResponsePayload::Success(payload),
        }
    }

    /// Builds a failed response for one request.
    pub fn failure(request: &ProviderRequest, error: ProviderRemoteError) -> Self {
        Self {
            protocol_version: PROVIDER_PROTOCOL_VERSION,
            request_id: request.request_id,
            payload: ProviderResponsePayload::Failure(error),
        }
    }

    /// Checks schema and request correlation before decoding provider bytes.
    pub fn validate_for(&self, request: &ProviderRequest) -> Result<(), ProviderTransportError> {
        request.validate()?;
        if self.protocol_version != PROVIDER_PROTOCOL_VERSION {
            return Err(ProviderTransportError::UnsupportedProtocol {
                version: self.protocol_version,
            });
        }
        if self.request_id != request.request_id {
            return Err(ProviderTransportError::MismatchedRequest {
                expected: request.request_id,
                actual: self.request_id,
            });
        }
        Ok(())
    }
}

/// Result body returned by a logical provider endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponsePayload {
    /// Provider-specific serialized result.
    Success(Vec<u8>),
    /// Stable provider error safe to cross a process or plugin boundary.
    Failure(ProviderRemoteError),
}

/// Structured error returned by a logical provider implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRemoteError {
    /// Stable provider-owned machine-readable code.
    pub code: String,
    /// Human-readable summary containing no secret material.
    pub message: String,
}

/// Delivery mechanism used by the host independently of provider semantics.
pub trait ProviderTransport {
    /// Returns the attachment category implemented by this transport.
    fn connection_kind(&self) -> ProviderConnectionKind;

    /// Delivers one serialized request and returns its serialized response.
    fn exchange(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderTransportError>;
}

/// Failure in request delivery or protocol correlation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderTransportError {
    /// Peer uses a provider protocol version unknown to this host.
    #[error("unsupported provider protocol version {version}")]
    UnsupportedProtocol {
        /// Rejected protocol version.
        version: u16,
    },
    /// Response belongs to another request.
    #[error("provider response request ID {actual} does not match {expected}")]
    MismatchedRequest {
        /// Request identity expected by the host.
        expected: u64,
        /// Request identity returned by the endpoint.
        actual: u64,
    },
    /// Connection-specific delivery failed before a valid response arrived.
    #[error("provider transport failed: {message}")]
    Delivery {
        /// Non-secret delivery error summary.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures plugin paths describe PDisks attachment rather than a C backend ABI.
    #[test]
    fn connection_kinds_remain_distinct() {
        let builtin = ProviderConnection::BuiltIn;
        let plugin = ProviderConnection::StabbyDynamicLibrary {
            library: PathBuf::from("/usr/lib64/pdisks/providers/libpdisks_luks.so"),
            entrypoint: "pdisks_provider_v1".to_owned(),
        };
        let process = ProviderConnection::Process {
            executable: PathBuf::from("/usr/libexec/pdisks-zfs-provider"),
        };

        assert_eq!(builtin.kind(), ProviderConnectionKind::BuiltIn);
        assert_eq!(plugin.kind(), ProviderConnectionKind::StabbyDynamicLibrary);
        assert_eq!(process.kind(), ProviderConnectionKind::Process);
    }

    /// Ensures response correlation is enforced before payload interpretation.
    #[test]
    fn response_rejects_another_request_id() {
        let request = ProviderRequest::new(
            41,
            ProviderId::new("luks").unwrap_or_else(|| unreachable!()),
            Vec::new(),
        );
        let response = ProviderResponse {
            protocol_version: PROVIDER_PROTOCOL_VERSION,
            request_id: 42,
            payload: ProviderResponsePayload::Success(Vec::new()),
        };

        assert_eq!(
            response.validate_for(&request),
            Err(ProviderTransportError::MismatchedRequest {
                expected: 41,
                actual: 42
            })
        );
    }

    /// Ensures the transport envelope survives serialization independently of actions.
    #[test]
    fn request_envelope_round_trips() {
        let request = ProviderRequest::new(
            7,
            ProviderId::new("luks").unwrap_or_else(|| unreachable!()),
            br#"{"open":{}}"#.to_vec(),
        );
        let bytes = serde_json::to_vec(&request)
            .unwrap_or_else(|error| panic!("serialize provider request: {error}"));
        let restored: ProviderRequest = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("deserialize provider request: {error}"));

        assert_eq!(restored, request);
    }
}
