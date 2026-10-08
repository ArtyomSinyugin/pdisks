//! Contracts owned by logical storage-technology providers.

use serde::{Deserialize, Serialize};
use storage_core::model::{Diagnostic, NodeGraph};

use crate::BackendId;

/// Stable identity of a logical provider such as `luks` or `lvm`.
#[repr(transparent)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    /// Creates a provider ID without whitespace.
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty() && !value.chars().any(char::is_whitespace)).then_some(Self(value))
    }

    /// Returns the opaque provider ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Technology semantics presented to validation and planning.
///
/// The associated action stays provider-owned. A future plugin transport must
/// carry its serialized representation without turning it into a C symbol or
/// a command line.
pub trait LogicalProvider {
    /// Typed action understood by this provider.
    type Action;

    /// Returns the stable logical-provider identity.
    fn id(&self) -> &ProviderId;

    /// Returns the concrete system backend selected for this provider.
    fn backend_id(&self) -> &BackendId;

    /// Checks technology-specific constraints without changing the system.
    fn validate_action(&self, action: &Self::Action, graph: &NodeGraph) -> Vec<Diagnostic>;
}
