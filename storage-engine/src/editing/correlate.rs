//! Conservative correlation of selected storage identities and uniquely backed partition tables.

use std::collections::HashSet;

use serde::Serialize;
use storage_core::model::{CurrentState, ExternalId, GraphError, Node, NodeId, NodeKind, Presence};
use thiserror::Error;

/// One stable match from an original/planned object to an actual observed object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeCorrelation {
    /// Identity in the original observed graph.
    pub source: NodeId,
    /// Identity in the new observed graph, independent of paths and display names.
    pub target: NodeId,
    /// Exact shared identity, or the uniquely backing disk identity for a partition table.
    pub identity: ExternalId,
}

/// A correlation failure leaves the original plan and both snapshots unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CorrelationError {
    /// A graph is structurally invalid before any identity is considered.
    #[error(transparent)]
    Structure(#[from] GraphError),
    /// The selected original node is absent.
    #[error("source node {0:?} is absent")]
    MissingSource(NodeId),
    /// A nil identity or non-present observation cannot identify an existing object.
    #[error("node {0:?} is not a present, non-nil observed object")]
    NotPresent(NodeId),
    /// The selected kind requires a supported durable UUID or WWN; serials and paths are insufficient.
    #[error("node {0:?} has no supported stable identity")]
    NoIdentity(NodeId),
    /// More than one original identity or original node could identify the selected object.
    #[error("source identity for node {0:?} is ambiguous")]
    AmbiguousSource(NodeId),
    /// No object with the exact original identity appears in the new graph.
    #[error("node {0:?} has no matching target identity")]
    MissingTarget(NodeId),
    /// Multiple targets, or two source objects targeting one node, are ambiguous.
    #[error("target identity for node {0:?} is ambiguous")]
    AmbiguousTarget(NodeId),
    /// The identity survived but the storage kind/layout changed incompatibly.
    #[error("node {source_node:?} changed kind at {target:?}")]
    KindChanged {
        /// Original observed object.
        source_node: NodeId,
        /// New object carrying the identity.
        target: NodeId,
    },
}

/// Matches only selected nodes with one supported identity unique in both complete graphs.
/// NodeId equality, device paths, names, serials, and probe generations never prove identity.
/// This is planning correlation, not authentication or permission to perform a live write.
pub fn correlate_nodes(
    current: &CurrentState,
    next: &CurrentState,
    selected: &[NodeId],
) -> Result<Vec<NodeCorrelation>, CorrelationError> {
    current.graph.validate_structure()?;
    next.graph.validate_structure()?;
    let mut seen = HashSet::new();
    let mut targets = HashSet::new();
    let mut result = Vec::new();
    for source in selected {
        if !seen.insert(*source) {
            continue;
        }
        let node = current
            .graph
            .node(source)
            .ok_or(CorrelationError::MissingSource(*source))?;
        present(*source, node)?;
        if matches!(node.kind.kind, NodeKind::PartitionTable(_)) {
            let binding = correlate_partition_table(current, next, *source)?;
            if !targets.insert(binding.target) {
                return Err(CorrelationError::AmbiguousTarget(*source));
            }
            result.push(binding);
            continue;
        }
        let identities: Vec<_> = node
            .size
            .identities
            .iter()
            .filter(|identity| supported_identity(node, identity))
            .collect();
        let identity = match identities.as_slice() {
            [] => return Err(CorrelationError::NoIdentity(*source)),
            [identity] => *identity,
            _ => return Err(CorrelationError::AmbiguousSource(*source)),
        };
        if node
            .size
            .identities
            .iter()
            .filter(|item| std::mem::discriminant(*item) == std::mem::discriminant(identity))
            .count()
            != 1
        {
            return Err(CorrelationError::AmbiguousSource(*source));
        }
        if current
            .graph
            .nodes()
            .filter(|(_, node)| node.size.identities.contains(identity))
            .count()
            != 1
        {
            return Err(CorrelationError::AmbiguousSource(*source));
        }
        let mut candidates = next
            .graph
            .nodes()
            .filter(|(_, node)| node.size.identities.contains(identity));
        let (target, candidate) = candidates
            .next()
            .ok_or(CorrelationError::MissingTarget(*source))?;
        if candidates.next().is_some() || !targets.insert(*target) {
            return Err(CorrelationError::AmbiguousTarget(*source));
        }
        present(*target, candidate)?;
        let compatible = match (&node.kind.kind, &candidate.kind.kind) {
            (NodeKind::LvmVg { .. }, NodeKind::LvmVg { .. })
            | (NodeKind::LvmPv, NodeKind::LvmPv)
            | (NodeKind::Disk, NodeKind::Disk)
            | (NodeKind::Partition { .. }, NodeKind::Partition { .. }) => true,
            (NodeKind::NvmeNamespace(left), NodeKind::NvmeNamespace(right)) => {
                left.format == right.format
            }
            (NodeKind::LvmLv(left), NodeKind::LvmLv(right)) => left.kind == right.kind,
            (NodeKind::Filesystem { kind: left, .. }, NodeKind::Filesystem { kind: right, .. }) => {
                left == right
            }
            _ => false,
        };
        if !compatible {
            return Err(CorrelationError::KindChanged {
                source_node: *source,
                target: *target,
            });
        }
        if candidate
            .size
            .identities
            .iter()
            .filter(|item| std::mem::discriminant(*item) == std::mem::discriminant(identity))
            .count()
            != 1
        {
            return Err(CorrelationError::AmbiguousTarget(*source));
        }
        result.push(NodeCorrelation {
            source: *source,
            target: *target,
            identity: identity.clone(),
        });
    }
    Ok(result)
}

/// A table has no UUID in this model, so require one table of the same type on a uniquely matched disk.
/// Only physical disk/namespace parents qualify; malformed graphs cannot recurse through table cycles.
fn correlate_partition_table(
    current: &CurrentState,
    next: &CurrentState,
    source: NodeId,
) -> Result<NodeCorrelation, CorrelationError> {
    use storage_core::model::DependencyKind;
    let mut parents = current
        .graph
        .dependencies()
        .filter(|edge| edge.to == source);
    let parent = parents.next().ok_or(CorrelationError::NoIdentity(source))?;
    if parents.next().is_some()
        || parent.kind != DependencyKind::Backs
        || !current.graph.node(&parent.from).is_some_and(|node| {
            matches!(node.kind.kind, NodeKind::Disk | NodeKind::NvmeNamespace(_))
        })
        || current
            .graph
            .dependencies()
            .filter(|edge| {
                edge.from == parent.from
                    && edge.kind == DependencyKind::Backs
                    && current
                        .graph
                        .node(&edge.to)
                        .is_some_and(|node| matches!(node.kind.kind, NodeKind::PartitionTable(_)))
            })
            .count()
            != 1
    {
        return Err(CorrelationError::AmbiguousSource(source));
    }
    let matched = correlate_nodes(current, next, &[parent.from])
        .map_err(|error| match error {
            CorrelationError::NoIdentity(_) => CorrelationError::NoIdentity(source),
            other => other,
        })?
        .pop()
        .ok_or(CorrelationError::NoIdentity(source))?;
    let mut tables = next
        .graph
        .dependencies()
        .filter(|edge| edge.from == matched.target && edge.kind == DependencyKind::Backs)
        .filter_map(|edge| next.graph.node(&edge.to).map(|node| (edge.to, node)))
        .filter(|(_, node)| matches!(node.kind.kind, NodeKind::PartitionTable(_)));
    let (target, node) = tables
        .next()
        .ok_or(CorrelationError::MissingTarget(source))?;
    if tables.next().is_some()
        || next
            .graph
            .dependencies()
            .filter(|edge| edge.to == target)
            .count()
            != 1
    {
        return Err(CorrelationError::AmbiguousTarget(source));
    }
    present(target, node)?;
    if current.graph.node(&source).map(|node| &node.kind.kind) != Some(&node.kind.kind) {
        return Err(CorrelationError::KindChanged {
            source_node: source,
            target,
        });
    }
    Ok(NodeCorrelation {
        source,
        target,
        identity: matched.identity,
    })
}

/// Rejects placeholders and nil canonical identities even when they carry matching UUID text.
fn present(id: NodeId, node: &Node) -> Result<(), CorrelationError> {
    if id.as_uuid().is_nil() || node.size.presence != Presence::Present {
        return Err(CorrelationError::NotPresent(id));
    }
    Ok(())
}

/// Accepts only durable UUID/WWN identities appropriate to the observed storage kind.
fn supported_identity(node: &Node, identity: &ExternalId) -> bool {
    let value = match (&node.kind.kind, identity) {
        (NodeKind::LvmVg { .. }, ExternalId::LvmVgUuid(value))
        | (NodeKind::LvmLv(_), ExternalId::LvmLvUuid(value))
        | (NodeKind::LvmPv, ExternalId::LvmPvUuid(value))
        | (NodeKind::Disk, ExternalId::Wwn(value)) => value,
        (NodeKind::Partition { .. }, ExternalId::GptPartitionUuid(uuid))
        | (NodeKind::NvmeNamespace(_), ExternalId::NvmeUuid(uuid)) => return !uuid.is_nil(),
        (NodeKind::NvmeNamespace(_), ExternalId::NvmeNguid(bytes)) => {
            return bytes.iter().any(|byte| *byte != 0);
        }
        (NodeKind::NvmeNamespace(_), ExternalId::NvmeEui64(bytes)) => {
            return bytes.iter().any(|byte| *byte != 0);
        }
        (NodeKind::Filesystem { kind, .. }, ExternalId::Filesystem { fs_type, value })
            if fs_type == kind.as_str() =>
        {
            value
        }
        _ => return false,
    };
    !value.trim().is_empty() && value.trim() == value
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use storage_core::model::{Bytes, FilesystemKind, LvmLv, LvmLvKind};

    /// Builds old/new VG snapshots with different NodeIds and names, sharing one durable UUID.
    pub(in crate::editing) fn snapshots() -> (CurrentState, CurrentState, NodeId, NodeId) {
        let value: serde_json::Value =
            serde_json::from_str(include_str!("../../examples/fixtures/probe_snapshot.json"))
                .unwrap_or_else(|error| panic!("fixture: {error}"));
        let current: CurrentState = serde_json::from_value(value["current"].clone())
            .unwrap_or_else(|error| panic!("current fixture: {error}"));
        let (source, node) = current.graph.nodes().next().unwrap_or_else(|| panic!("VG"));
        let source = *source;
        let mut node = node.clone();
        node.kind.kind = NodeKind::LvmVg {
            name: "renamed".to_owned(),
            extent_size: Some(Bytes::new(4 * 1024 * 1024)),
        };
        let target = NodeId::new();
        let mut next = current.clone();
        next.graph = storage_core::model::NodeGraph::new();
        next.graph
            .insert_new(target, node)
            .unwrap_or_else(|error| panic!("next VG: {error}"));
        (current, next, source, target)
    }

    /// Renames and transient IDs are independent of identity; only selected objects are matched.
    #[test]
    fn correlation_uses_unique_identity_instead_of_names_and_node_ids() {
        let (current, next, source, target) = snapshots();
        let result = correlate_nodes(&current, &next, &[source, source])
            .unwrap_or_else(|error| panic!("correlate: {error}"));
        assert_eq!(result.len(), 1);
        assert_eq!((result[0].source, result[0].target), (source, target));
        assert!(
            correlate_nodes(&current, &next, &[])
                .unwrap_or_else(|error| panic!("empty: {error}"))
                .is_empty()
        );
        let mut reused = current.clone();
        let mut node = reused
            .graph
            .node(&source)
            .cloned()
            .unwrap_or_else(|| panic!("node"));
        node.size.identities = vec![ExternalId::LvmVgUuid("replacement".to_owned())];
        reused
            .graph
            .replace_existing(source, node)
            .unwrap_or_else(|error| panic!("replace: {error}"));
        assert_eq!(
            correlate_nodes(&current, &reused, &[source]),
            Err(CorrelationError::MissingTarget(source))
        );
    }

    /// LV layouts and filesystem types remain compatible, while labels and names may change.
    #[test]
    fn lv_and_filesystem_matches_reject_incompatible_types() {
        for (kind, identity, incompatible) in [
            (
                NodeKind::LvmLv(LvmLv {
                    name: "data".to_owned(),
                    kind: LvmLvKind::Linear,
                }),
                ExternalId::LvmLvUuid("lv-uuid".to_owned()),
                NodeKind::LvmLv(LvmLv {
                    name: "data".to_owned(),
                    kind: LvmLvKind::ThinPool,
                }),
            ),
            (
                NodeKind::Filesystem {
                    kind: FilesystemKind::Ext4,
                    label: None,
                },
                ExternalId::Filesystem {
                    fs_type: "ext4".to_owned(),
                    value: "fs-uuid".to_owned(),
                },
                NodeKind::Filesystem {
                    kind: FilesystemKind::Xfs,
                    label: None,
                },
            ),
        ] {
            let (mut current, mut next, source, target) = snapshots();
            let mut node = current
                .graph
                .node(&source)
                .cloned()
                .unwrap_or_else(|| panic!("source"));
            node.kind.kind = kind;
            node.size.identities = vec![identity];
            current
                .graph
                .replace_existing(source, node.clone())
                .unwrap_or_else(|error| panic!("source kind: {error}"));
            next.graph
                .replace_existing(target, node.clone())
                .unwrap_or_else(|error| panic!("target kind: {error}"));
            assert!(correlate_nodes(&current, &next, &[source]).is_ok());
            node.kind.kind = incompatible;
            next.graph
                .replace_existing(target, node)
                .unwrap_or_else(|error| panic!("changed kind: {error}"));
            assert!(matches!(
                correlate_nodes(&current, &next, &[source]),
                Err(CorrelationError::KindChanged { .. })
            ));
        }
    }

    /// Invalid extra UUID records cannot be hidden beside one apparently usable identity.
    #[test]
    fn contradictory_identity_metadata_is_rejected() {
        let (mut current, mut next, source, target) = snapshots();
        let mut node = current
            .graph
            .node(&source)
            .cloned()
            .unwrap_or_else(|| panic!("source"));
        node.size
            .identities
            .push(ExternalId::LvmVgUuid(String::new()));
        current
            .graph
            .replace_existing(source, node)
            .unwrap_or_else(|error| panic!("source metadata: {error}"));
        assert_eq!(
            correlate_nodes(&current, &next, &[source]),
            Err(CorrelationError::AmbiguousSource(source))
        );
        let (current, _, _, _) = snapshots();
        // Reuse the same source graph only for an explicit target metadata conflict.
        let source = *current
            .graph
            .nodes()
            .next()
            .unwrap_or_else(|| panic!("source"))
            .0;
        let mut node = next
            .graph
            .node(&target)
            .cloned()
            .unwrap_or_else(|| panic!("target"));
        node.size
            .identities
            .push(ExternalId::LvmVgUuid(String::new()));
        next.graph
            .replace_existing(target, node)
            .unwrap_or_else(|error| panic!("target metadata: {error}"));
        assert_eq!(
            correlate_nodes(&current, &next, &[source]),
            Err(CorrelationError::AmbiguousTarget(source))
        );
        let mut wrong_type = current.clone();
        let mut node = wrong_type
            .graph
            .node(&source)
            .cloned()
            .unwrap_or_else(|| panic!("source"));
        node.kind.kind = NodeKind::Filesystem {
            kind: FilesystemKind::Ext4,
            label: None,
        };
        node.size.identities = vec![ExternalId::Filesystem {
            fs_type: "xfs".to_owned(),
            value: "fs-uuid".to_owned(),
        }];
        wrong_type
            .graph
            .replace_existing(source, node)
            .unwrap_or_else(|error| panic!("filesystem metadata: {error}"));
        assert_eq!(
            correlate_nodes(&wrong_type, &next, &[source]),
            Err(CorrelationError::NoIdentity(source))
        );
    }

    /// Cloned, missing, weak, non-present, and incompatible identities all fail closed.
    #[test]
    fn ambiguous_or_incompatible_observations_are_rejected() {
        for case in 0..7 {
            let (mut current, mut next, source, target) = snapshots();
            match case {
                0 => {
                    let node = next
                        .graph
                        .node(&target)
                        .cloned()
                        .unwrap_or_else(|| panic!("target"));
                    next.graph
                        .insert_new(NodeId::new(), node)
                        .unwrap_or_else(|error| panic!("clone: {error}"));
                }
                1 => {
                    let node = current
                        .graph
                        .node(&source)
                        .cloned()
                        .unwrap_or_else(|| panic!("source"));
                    current
                        .graph
                        .insert_new(NodeId::new(), node)
                        .unwrap_or_else(|error| panic!("clone: {error}"));
                }
                2 => {
                    let mut node = current
                        .graph
                        .node(&source)
                        .cloned()
                        .unwrap_or_else(|| panic!("source"));
                    node.size.identities = vec![ExternalId::Serial {
                        vendor: None,
                        value: "serial".to_owned(),
                    }];
                    current
                        .graph
                        .replace_existing(source, node)
                        .unwrap_or_else(|error| panic!("weak: {error}"));
                }
                3 => {
                    let mut node = next
                        .graph
                        .node(&target)
                        .cloned()
                        .unwrap_or_else(|| panic!("target"));
                    node.size.presence = Presence::Missing;
                    next.graph
                        .replace_existing(target, node)
                        .unwrap_or_else(|error| panic!("missing: {error}"));
                }
                4 => {
                    let mut node = next
                        .graph
                        .node(&target)
                        .cloned()
                        .unwrap_or_else(|| panic!("target"));
                    node.kind.kind = NodeKind::Disk;
                    next.graph
                        .replace_existing(target, node)
                        .unwrap_or_else(|error| panic!("kind: {error}"));
                }
                5 => {
                    let mut node = current
                        .graph
                        .node(&source)
                        .cloned()
                        .unwrap_or_else(|| panic!("source"));
                    node.size.identities.push(node.size.identities[0].clone());
                    current
                        .graph
                        .replace_existing(source, node)
                        .unwrap_or_else(|error| panic!("duplicate: {error}"));
                }
                _ => next.graph = storage_core::model::NodeGraph::new(),
            }
            assert!(
                correlate_nodes(&current, &next, &[source]).is_err(),
                "case {case}"
            );
        }
        let (current, next, _, _) = snapshots();
        let missing = NodeId::new();
        assert_eq!(
            correlate_nodes(&current, &next, &[missing]),
            Err(CorrelationError::MissingSource(missing))
        );
    }
}
