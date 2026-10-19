//! Conservative ext4 output correlation; evidence inspection alone never authorizes a write.

use storage_core::model::{DependencyKind, ExternalId, FilesystemKind, NodeId, NodeKind, Presence};
use storage_engine::{
    LiveObservation,
    editing::{NodeCorrelation, correlate_nodes},
};
use thiserror::Error;
use uuid::Uuid;

/// Missing or contradictory filesystem evidence keeps the active attempt unresolved.
#[derive(Debug, Error)]
pub enum FilesystemEvidenceError {
    /// Publications are malformed or do not establish a new ext4 UUID on the admitted backing.
    #[error("new ext4 identity or backing is not established by fresh observations")]
    Identity,
}

/// Correlates a new ext4 result with exactly one Backs edge from the admitted LV.
/// The caller must retain the actual pre/post publications and establish dispatch provenance.
/// Neither a saved snapshot nor this return value can insert a verified runtime binding.
pub fn correlate_created_ext4(
    before: &LiveObservation,
    after: &LiveObservation,
    planned: NodeId,
    backing: NodeId,
    uuid: &str,
    label: &Option<String>,
) -> Result<NodeCorrelation, FilesystemEvidenceError> {
    let invalid = || FilesystemEvidenceError::Identity;
    before.validate().map_err(|_| invalid())?;
    after.validate().map_err(|_| invalid())?;
    if planned.as_uuid().is_nil()
        || before.snapshot.graph.node(&planned).is_some()
        || before.reference.probe_instance != after.reference.probe_instance
        || before.reference.epoch >= after.reference.epoch
        || Uuid::parse_str(uuid)
            .ok()
            .is_none_or(|id| id.is_nil() || id.hyphenated().to_string() != uuid)
    {
        return Err(invalid());
    }
    let identity = ExternalId::Filesystem {
        fs_type: "ext4".into(),
        value: uuid.into(),
    };
    if before
        .snapshot
        .graph
        .nodes()
        .any(|(_, node)| node.size.identities.contains(&identity))
    {
        return Err(invalid());
    }
    let mapping =
        correlate_nodes(&before.snapshot, &after.snapshot, &[backing]).map_err(|_| invalid())?;
    let actual_backing = mapping.first().ok_or_else(invalid)?.target;
    if !before
        .snapshot
        .graph
        .node(&backing)
        .is_some_and(|node| matches!(node.kind.kind, NodeKind::LvmLv(_)))
    {
        return Err(invalid());
    }
    let mut matches = after
        .snapshot
        .graph
        .nodes()
        .filter(|(_, node)| node.size.identities.contains(&identity));
    let (id, node) = matches.next().ok_or_else(invalid)?;
    if matches.next().is_some()
        || node.size.presence != Presence::Present
        || node.kind.kind
            != (NodeKind::Filesystem {
                kind: FilesystemKind::Ext4,
                label: label.clone(),
            })
        || node
            .size
            .identities
            .iter()
            .filter(|id| matches!(id, ExternalId::Filesystem { .. }))
            .count()
            != 1
        || after
            .snapshot
            .graph
            .dependencies()
            .any(|edge| edge.from == *id)
        || after
            .snapshot
            .graph
            .relations()
            .any(|edge| edge.from == *id || edge.to == *id)
    {
        return Err(invalid());
    }
    let parents: Vec<_> = after
        .snapshot
        .graph
        .dependencies()
        .filter(|edge| edge.to == *id)
        .collect();
    if !matches!(parents.as_slice(), [edge] if edge.from == actual_backing && edge.kind == DependencyKind::Backs)
    {
        return Err(invalid());
    }
    Ok(NodeCorrelation {
        source: planned,
        target: *id,
        identity,
    })
}
