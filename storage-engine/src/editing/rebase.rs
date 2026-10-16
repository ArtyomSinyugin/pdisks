//! Explicit draft translation between immutable observation contexts; no system actions occur.

use std::collections::{HashMap, HashSet};

use storage_core::model::{MountContext, MountSource, NodeId};
use thiserror::Error;

use crate::HistoricalBaseline;

use super::{
    CorrelationError, DraftBuildError, NodeCorrelation, OperationId, PlanDraft, PlanRequest,
    build::validate_draft, build_preview, correlate_nodes,
};

/// Rejected baseline translation; original requests and observations are left untouched.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RebaseError {
    /// A new context must not reuse the immutable historical baseline identity.
    #[error("rebase requires a new baseline identity")]
    SameBaselineIdentity,
    /// Baseline, request, or output identity validation failed.
    #[error("invalid rebase inputs: {0}")]
    Inputs(Box<DraftBuildError>),
    /// Selected observed objects cannot be uniquely correlated.
    #[error(transparent)]
    Correlation(#[from] CorrelationError),
    /// A still-planned output already exists in the new observed state.
    #[error("planned output {0:?} already exists in the new baseline")]
    OutputCollision(NodeId),
    /// Path-only removal or an unsupported mount source cannot be safely rebound yet.
    #[error("operation {0:?} has no supported rebase recipe")]
    UnsupportedOperation(OperationId),
    /// Non-host namespaces/target roots have no durable identity in the initial rebase API.
    #[error("mount context {0:?} cannot be correlated across baselines")]
    UnsupportedMountContext(MountContext),
}

/// Copies a draft, correlating existing inputs and retaining operation/planned-output identities.
/// The returned draft must still be simulated and reviewed; this does not modify a session.
pub fn remap_draft(
    current: &HistoricalBaseline,
    draft: &PlanDraft,
    next: &HistoricalBaseline,
) -> Result<(PlanDraft, Vec<NodeCorrelation>), RebaseError> {
    if current.id == next.id {
        return Err(RebaseError::SameBaselineIdentity);
    }
    validate_draft(current, draft).map_err(|error| RebaseError::Inputs(Box::new(error)))?;
    build_preview(next, &PlanDraft::default())
        .map_err(|error| RebaseError::Inputs(Box::new(error)))?;
    let outputs: HashSet<_> = draft
        .operations
        .iter()
        .filter_map(|operation| operation.created_node)
        .collect();
    for operation in &draft.operations {
        if let Some(output) = operation.created_node
            && next.snapshot.graph.node(&output).is_some()
        {
            return Err(RebaseError::OutputCollision(output));
        }
        match &operation.request {
            PlanRequest::Unmount { .. } | PlanRequest::RemovePersistentMount { .. } => {
                return Err(RebaseError::UnsupportedOperation(operation.id));
            }
            PlanRequest::Mount(entry) | PlanRequest::PersistMount(entry) => {
                if entry.context != MountContext::Host {
                    return Err(RebaseError::UnsupportedMountContext(entry.context.clone()));
                }
                if !matches!(
                    entry.source,
                    MountSource::Filesystem(_) | MountSource::Tmpfs
                ) {
                    return Err(RebaseError::UnsupportedOperation(operation.id));
                }
            }
            _ => {}
        }
    }
    for protected in draft
        .protected_scope
        .runtime_mounts
        .iter()
        .chain(&draft.protected_scope.persistent_mounts)
    {
        if protected.context != MountContext::Host {
            return Err(RebaseError::UnsupportedMountContext(
                protected.context.clone(),
            ));
        }
    }
    let selected: Vec<_> = draft
        .operations
        .iter()
        .flat_map(|operation| operation.request.referenced_nodes())
        .chain(draft.protected_scope.nodes.iter().copied())
        .filter(|node| !outputs.contains(node))
        .collect();
    let correlations = correlate_nodes(&current.snapshot, &next.snapshot, &selected)?;
    let mapping: HashMap<_, _> = correlations
        .iter()
        .map(|binding| (binding.source, binding.target))
        .collect();
    let mut translated = draft.clone();
    for operation in &mut translated.operations {
        operation.request.remap_nodes(&mapping);
    }
    for node in &mut translated.protected_scope.nodes {
        if let Some(target) = mapping.get(node) {
            *node = *target;
        }
    }
    validate_draft(next, &translated).map_err(|error| RebaseError::Inputs(Box::new(error)))?;
    Ok((translated, correlations))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{
        CapacityEvidence,
        editing::{PlanOperation, PlanSession, correlate::tests::snapshots},
    };
    use storage_core::model::{Bytes, FilesystemKind};
    use storage_provider::{AllocationDomain, AllocationQuantity};

    /// Builds an LV/FS/mount draft with a unique observed VG and new baseline NodeId.
    pub(in crate::editing) fn session_and_baseline()
    -> (PlanSession, HistoricalBaseline, NodeId, NodeId) {
        let (current, next, source, target) = snapshots();
        let old = HistoricalBaseline {
            id: uuid::Uuid::now_v7(),
            snapshot: current,
            capacity_evidence: vec![CapacityEvidence {
                domain: AllocationDomain::LvmVolumeGroup(source),
                available: AllocationQuantity::new(100),
            }],
        };
        let next = HistoricalBaseline {
            id: uuid::Uuid::now_v7(),
            snapshot: next,
            capacity_evidence: vec![CapacityEvidence {
                domain: AllocationDomain::LvmVolumeGroup(target),
                available: AllocationQuantity::new(80),
            }],
        };
        let mut session =
            PlanSession::new(old).unwrap_or_else(|error| panic!("old session: {error}"));
        let lv = session
            .add_lv(
                session.revision(),
                source,
                "data",
                Bytes::new(20 * 4 * 1024 * 1024),
            )
            .unwrap_or_else(|error| panic!("LV: {error}"));
        let backing = lv.created_node.unwrap_or_else(|| panic!("LV ID"));
        let filesystem = session
            .add_filesystem(session.revision(), backing, FilesystemKind::Ext4)
            .unwrap_or_else(|error| panic!("FS: {error}"));
        let filesystem = filesystem.created_node.unwrap_or_else(|| panic!("FS ID"));
        session
            .mount(
                session.revision(),
                storage_core::model::MountEntry {
                    source: MountSource::Filesystem(filesystem),
                    target: "/data".into(),
                    options: vec!["rw".to_owned()],
                    context: MountContext::Host,
                },
            )
            .unwrap_or_else(|error| panic!("mount: {error}"));
        (session, next, source, target)
    }

    /// Translation preserves planned IDs, options, protection scope, and original snapshots.
    #[test]
    fn draft_remapping_changes_only_existing_inputs() {
        let (session, next, source, target) = session_and_baseline();
        let mut draft = session.draft().clone();
        draft.protected_scope.nodes = vec![
            source,
            draft.operations[0]
                .created_node
                .unwrap_or_else(|| panic!("output")),
        ];
        let original = draft.clone();
        let (translated, correlations) = remap_draft(session.baseline(), &draft, &next)
            .unwrap_or_else(|error| panic!("remap: {error}"));
        assert_eq!(correlations.len(), 1);
        assert_eq!(
            (correlations[0].source, correlations[0].target),
            (source, target)
        );
        assert!(
            matches!(translated.operations[0].request, PlanRequest::CreateLv { vg, .. } if vg == target)
        );
        assert_eq!(translated.operations[1..], draft.operations[1..]);
        assert_eq!(translated.protected_scope.nodes[0], target);
        assert_eq!(
            translated.protected_scope.nodes[1],
            draft.protected_scope.nodes[1]
        );
        for (before, after) in draft.operations.iter().zip(&translated.operations) {
            assert_eq!(before.id, after.id);
            assert_eq!(before.created_node, after.created_node);
        }
        assert_eq!(draft, original);
    }

    /// Output collisions, reused contexts, and path-only requests cannot silently change meaning.
    #[test]
    fn remapping_rejects_collisions_and_unsupported_contexts() {
        let (session, mut next, _, target) = session_and_baseline();
        assert_eq!(
            remap_draft(session.baseline(), session.draft(), session.baseline()),
            Err(RebaseError::SameBaselineIdentity)
        );
        let output = session.draft().operations[0]
            .created_node
            .unwrap_or_else(|| panic!("output"));
        let node = next
            .snapshot
            .graph
            .node(&target)
            .cloned()
            .unwrap_or_else(|| panic!("VG"));
        next.snapshot
            .graph
            .insert_new(output, node)
            .unwrap_or_else(|error| panic!("collision: {error}"));
        assert_eq!(
            remap_draft(session.baseline(), session.draft(), &next),
            Err(RebaseError::OutputCollision(output))
        );
        let (session, next, _, _) = session_and_baseline();
        let mut draft = session.draft().clone();
        let operation = PlanOperation::new(PlanRequest::Unmount {
            target: "/data".into(),
            context: MountContext::Host,
        });
        let id = operation.id;
        draft.operations.push(operation);
        assert_eq!(
            remap_draft(session.baseline(), &draft, &next),
            Err(RebaseError::UnsupportedOperation(id))
        );
        draft.operations.pop();
        if let PlanRequest::Mount(entry) = &mut draft.operations[2].request {
            entry.context = MountContext::Namespace("ephemeral".to_owned());
        }
        assert!(matches!(
            remap_draft(session.baseline(), &draft, &next),
            Err(RebaseError::UnsupportedMountContext(_))
        ));
    }
}
