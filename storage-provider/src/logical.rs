//! Contracts owned by logical storage-technology providers.

use serde::{Deserialize, Serialize};
use storage_core::model::{Diagnostic, NodeGraph};

use crate::BackendId;

/// Stable identity of a logical provider such as `luks` or `lvm`.
#[repr(transparent)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
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

impl<'de> Deserialize<'de> for ProviderId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| {
            <D::Error as serde::de::Error>::custom(
                "provider ID must be non-empty and contain no whitespace",
            )
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that JSON cannot bypass provider-ID constructor invariants.
    #[test]
    fn provider_id_rejects_invalid_json() {
        assert!(serde_json::from_str::<ProviderId>(r#""""#).is_err());
        assert!(serde_json::from_str::<ProviderId>(r#""two words""#).is_err());
    }
}
