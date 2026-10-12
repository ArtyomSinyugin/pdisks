//! Contracts owned by logical storage-technology providers.

use serde::{Deserialize, Serialize};
use storage_core::model::{
    DependencyKind, Diagnostic, DiagnosticSeverity, DiagnosticSubject, NodeGraph, NodeId, NodeKind,
};

use crate::{BackendId, PartitionAction, ProviderAction};

/// Runs backend-independent technology checks against the effective graph.
///
/// LUKS shares the same checks with `LogicalProvider::validate_action`.
/// Partition mutations require an unambiguous table metadata domain.
/// Other technologies currently rely on their action-contract prerequisites;
/// their parameter validators belong here as they are implemented.
pub fn validate_provider_action(action: &ProviderAction, graph: &NodeGraph) -> Vec<Diagnostic> {
    match action {
        ProviderAction::Luks(action) => crate::luks::validate_action(action, graph),
        ProviderAction::Partition(
            PartitionAction::Resize { partition, .. }
            | PartitionAction::Delete { partition }
            | PartitionAction::SetAttributes { partition, .. },
        ) => {
            if partition_table(graph, *partition).is_some() {
                Vec::new()
            } else {
                vec![Diagnostic {
                    code: "partition.action.invalid_table_ownership".to_owned(),
                    severity: DiagnosticSeverity::Error,
                    subjects: vec![DiagnosticSubject::Node(*partition)],
                    message: "partition mutation requires exactly one containing partition table"
                        .to_owned(),
                    evidence: None,
                    suggested_remedy: None,
                }]
            }
        }
        ProviderAction::Partition(
            PartitionAction::CreateTable { .. } | PartitionAction::Create { .. },
        )
        | ProviderAction::Lvm(_)
        | ProviderAction::Filesystem(_)
        | ProviderAction::Btrfs(_)
        | ProviderAction::Mount(_) => Vec::new(),
    }
}

/// Resolves exactly one containing partition table without choosing a hash iteration order.
pub(crate) fn partition_table(graph: &NodeGraph, partition: NodeId) -> Option<NodeId> {
    let mut owners = graph.dependencies().filter(|dependency| {
        dependency.to == partition && matches!(dependency.kind, DependencyKind::Contains { .. })
    });
    if let Some(owner) = owners.next()
        && owners.next().is_none()
        && graph
            .node(&owner.from)
            .is_some_and(|node| matches!(node.kind.kind, NodeKind::PartitionTable(_)))
    {
        return Some(owner.from);
    }
    None
}

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
    use storage_core::model::{
        Bytes, Dependency, Node, NodeFacts, NodeSpec, PartitionAttributes, PartitionTable,
    };

    use super::*;

    /// Unknown, invalid, or competing ownership cannot bypass partition metadata locking.
    #[test]
    fn partition_mutations_require_one_valid_metadata_domain() {
        let partition = NodeId::new();
        let first = NodeId::new();
        let second = NodeId::new();
        for owner_count in [0, 1, 2] {
            for valid_kind in [false, true] {
                let mut graph = NodeGraph::new();
                for table in [first, second].into_iter().take(owner_count) {
                    graph.insert_node(
                        table,
                        Node {
                            kind: NodeSpec {
                                kind: if valid_kind {
                                    NodeKind::PartitionTable(PartitionTable::Gpt)
                                } else {
                                    NodeKind::Disk
                                },
                                size: None,
                            },
                            size: NodeFacts::default(),
                        },
                    );
                    graph.insert_dependency(Dependency {
                        from: table,
                        to: partition,
                        kind: DependencyKind::Contains {
                            offset: Bytes::new(0),
                        },
                    });
                }
                for operation in [
                    PartitionAction::Delete { partition },
                    PartitionAction::Resize {
                        partition,
                        new_size: Bytes::new(60),
                    },
                    PartitionAction::SetAttributes {
                        partition,
                        attributes: PartitionAttributes::default(),
                    },
                ] {
                    let diagnostics =
                        validate_provider_action(&ProviderAction::Partition(operation), &graph);
                    if owner_count == 1 && valid_kind {
                        assert!(diagnostics.is_empty());
                        assert_eq!(partition_table(&graph, partition), Some(first));
                    } else {
                        assert!(matches!(diagnostics.as_slice(), [diagnostic]
                            if diagnostic.code == "partition.action.invalid_table_ownership"
                                && diagnostic.severity == DiagnosticSeverity::Error));
                    }
                }
            }
        }
    }

    /// Verifies that JSON cannot bypass provider-ID constructor invariants.
    #[test]
    fn provider_id_rejects_invalid_json() {
        assert!(serde_json::from_str::<ProviderId>(r#""""#).is_err());
        assert!(serde_json::from_str::<ProviderId>(r#""two words""#).is_err());
    }
}
