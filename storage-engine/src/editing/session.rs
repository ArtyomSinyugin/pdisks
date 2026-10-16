//! Revision-checked editing and draft-only history against an immutable baseline.

use std::{collections::HashSet, sync::Arc};

use storage_core::model::{Bytes, FilesystemKind, LvmLvKind, MountEntry, NodeId};
use thiserror::Error;
use uuid::Uuid;

use crate::{HistoricalBaseline, ProtectedScope};

use super::{
    DraftBuildError, DraftDocument, DraftDocumentError, DraftRevision, MAX_DRAFT_OPERATIONS,
    OperationId, PlanDraft, PlanOperation, PlanRequest, PreviewState, RebaseError, RebasePreview,
    build_preview,
};

/// Default number of draft snapshots retained on each side of undo/redo history.
pub const DEFAULT_HISTORY_LIMIT: usize = 64;

/// One user command; valid edits may deliberately leave an invalid, editable plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanEdit {
    /// Appends a request and reserves any created node exactly once.
    Add(PlanRequest),
    /// Replaces parameters while retaining the request kind and stable identities.
    Replace {
        /// Existing request to change.
        operation: OperationId,
        /// New parameters of the same request kind.
        request: PlanRequest,
    },
    /// Cancels requests; all transitively dependent requests must be explicitly included.
    Remove {
        /// Stable request IDs chosen by the caller.
        operations: Vec<OperationId>,
    },
    /// Moves one request in the explicit user order, without bypassing required DAG edges.
    MoveBefore {
        /// Existing request to move.
        operation: OperationId,
        /// Request to precede, or the end of the list when absent.
        before: Option<OperationId>,
    },
    /// Replaces the explicit preservation policy.
    SetProtection(ProtectedScope),
    /// Cancels every request while retaining the preservation policy and baseline.
    Clear,
}

/// Confirmation of draft publication, not confirmation of execution eligibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditReceipt {
    /// New revision shared by the draft and its current preview.
    pub revision: DraftRevision,
    /// Identity allocated by an Add command.
    pub added_operation: Option<OperationId>,
    /// Output node allocated by an Add command that creates an object.
    pub created_node: Option<NodeId>,
}

/// A rejected command leaves the draft, preview, revision, and both history stacks unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EditError {
    /// Another accepted command already changed this session.
    #[error("expected revision {expected:?}, actual revision {actual:?}")]
    RevisionConflict {
        /// Revision supplied by the caller.
        expected: DraftRevision,
        /// Current session revision.
        actual: DraftRevision,
    },
    /// An addressed request is absent from the current draft.
    #[error("unknown operation {0:?}")]
    UnknownOperation(OperationId),
    /// Changing request kind requires removing the old request and adding a new one.
    #[error("cannot replace the kind of operation {0:?}")]
    RequestKindChange(OperationId),
    /// The LV size convenience method requires a planned CreateLv request.
    #[error("operation {0:?} is not a planned LV creation")]
    NotCreatedLv(OperationId),
    /// Geometry editing requires a planned CreatePartition request.
    #[error("operation {0:?} is not a planned partition creation")]
    NotCreatedPartition(OperationId),
    /// Cancelling a creator requires an explicit decision about its dependent requests.
    #[error("removal requires dependent operations {operations:?}")]
    DependentOperations {
        /// Transitive dependents omitted by the caller, in current user order.
        operations: Vec<OperationId>,
    },
    /// No older editable draft is available.
    #[error("nothing to undo")]
    NoUndo,
    /// No undone editable draft is available.
    #[error("nothing to redo")]
    NoRedo,
    /// The monotonic revision counter cannot advance safely.
    #[error("draft revision exhausted")]
    RevisionExhausted,
    /// Adding another request would exceed the bounded editor input size.
    #[error("draft contains the maximum {MAX_DRAFT_OPERATIONS} operations")]
    OperationLimit,
    /// A programming/contract failure prevented publishing the candidate draft.
    #[error("internal preview build failure: {0}")]
    InternalBuild(Box<DraftBuildError>),
}

/// One local editing session; state observation and real execution remain separate boundaries.
#[derive(Debug)]
pub struct PlanSession {
    /// Identity for views and future saved draft documents.
    id: Uuid,
    /// Monotonic publication version, independent of undo position.
    revision: DraftRevision,
    /// Shared immutable observed state and capacity from the same probe context.
    baseline: Arc<HistoricalBaseline>,
    /// Sole editable list of user requests.
    draft: PlanDraft,
    /// Current result only; never an older successful result after an invalid edit.
    preview: PreviewState,
    /// Configurable snapshot bound; zero disables undo/redo retention.
    history_limit: usize,
    /// Prior user drafts, without inverse predicted effects.
    undo: Vec<PlanDraft>,
    /// Drafts removed by undo, cleared by a new edit.
    redo: Vec<PlanDraft>,
}

impl PlanSession {
    /// Validates the supplied baseline by building the initial empty plan without native calls.
    pub fn new(baseline: HistoricalBaseline) -> Result<Self, DraftBuildError> {
        let draft = PlanDraft::default();
        let preview = PreviewState::Calculated(Box::new(build_preview(&baseline, &draft)?));
        Ok(Self {
            id: Uuid::now_v7(),
            revision: DraftRevision::default(),
            baseline: Arc::new(baseline),
            draft,
            preview,
            history_limit: DEFAULT_HISTORY_LIMIT,
            undo: Vec::new(),
            redo: Vec::new(),
        })
    }

    /// Opens bounded editable inputs and recomputes the current preview, preserving stable IDs.
    /// Semantic plan failures remain editable; persisted forecasts and history are never trusted.
    pub fn from_document(document: DraftDocument) -> Result<Self, DraftDocumentError> {
        document.validate()?;
        let result = build_preview(&document.baseline, &document.draft);
        if let Err(error) = &result
            && error.is_internal()
        {
            return Err(DraftDocumentError::Inputs(Box::new(error.clone())));
        }
        Ok(Self {
            id: document.session_id,
            revision: document.revision,
            baseline: Arc::new(document.baseline),
            draft: document.draft,
            preview: result.into(),
            history_limit: DEFAULT_HISTORY_LIMIT,
            undo: Vec::new(),
            redo: Vec::new(),
        })
    }

    /// Returns this session's identity, separate from any frozen plan or execution run.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Returns the version required when submitting an edit, undo, or redo.
    pub fn revision(&self) -> DraftRevision {
        self.revision
    }

    /// Returns the immutable observed context; fresh probing must not silently replace it.
    pub fn baseline(&self) -> &HistoricalBaseline {
        &self.baseline
    }

    /// Returns typed requests, including semantically invalid requests awaiting correction.
    pub fn draft(&self) -> &PlanDraft {
        &self.draft
    }

    /// Returns the result for the current revision only.
    pub fn preview(&self) -> &PreviewState {
        &self.preview
    }

    /// Returns the maximum snapshots retained in each history stack.
    pub fn history_limit(&self) -> usize {
        self.history_limit
    }

    /// Changes retention without changing the draft/revision; zero discards all history.
    pub fn set_history_limit(&mut self, limit: usize) {
        self.history_limit = limit;
        for history in [&mut self.undo, &mut self.redo] {
            let discard = history.len().saturating_sub(limit);
            drop(history.drain(..discard));
        }
    }

    /// Applies a command atomically and recalculates from the same baseline.
    pub fn edit(
        &mut self,
        expected_revision: DraftRevision,
        edit: PlanEdit,
    ) -> Result<EditReceipt, EditError> {
        let revision = self.check_revision(expected_revision)?;
        let mut candidate = self.draft.clone();
        let mut receipt = EditReceipt {
            revision,
            added_operation: None,
            created_node: None,
        };
        match edit {
            PlanEdit::Add(request) => {
                if candidate.operations.len() >= MAX_DRAFT_OPERATIONS {
                    return Err(EditError::OperationLimit);
                }
                let operation = PlanOperation::new(request);
                receipt.added_operation = Some(operation.id);
                receipt.created_node = operation.created_node;
                candidate.operations.push(operation);
            }
            PlanEdit::Replace { operation, request } => {
                let index = operation_index(&candidate, operation)?;
                let previous = &candidate.operations[index].request;
                let same_kind = std::mem::discriminant(previous)
                    == std::mem::discriminant(&request)
                    && match (previous, &request) {
                        (PlanRequest::Maintenance(left), PlanRequest::Maintenance(right)) => {
                            std::mem::discriminant(left) == std::mem::discriminant(right)
                        }
                        _ => true,
                    };
                if !same_kind {
                    return Err(EditError::RequestKindChange(operation));
                }
                candidate.operations[index].request = request;
            }
            PlanEdit::Remove { operations } => remove_operations(&mut candidate, &operations)?,
            PlanEdit::MoveBefore { operation, before } => {
                let index = operation_index(&candidate, operation)?;
                if let Some(before) = before {
                    operation_index(&candidate, before)?;
                }
                if before != Some(operation) {
                    let operation = candidate.operations.remove(index);
                    let target = match before {
                        Some(before) => operation_index(&candidate, before)?,
                        None => candidate.operations.len(),
                    };
                    candidate.operations.insert(target, operation);
                }
            }
            PlanEdit::SetProtection(protected) => candidate.protected_scope = protected,
            PlanEdit::Clear => candidate.operations.clear(),
        }
        let preview = self.build_candidate(&candidate)?;
        push_history(
            &mut self.undo,
            std::mem::replace(&mut self.draft, candidate),
            self.history_limit,
        );
        self.redo.clear();
        self.preview = preview;
        self.revision = revision;
        Ok(receipt)
    }

    /// Adds a linear logical volume with activation enabled; other layouts use `edit(Add(...))`.
    pub fn add_lv(
        &mut self,
        expected_revision: DraftRevision,
        vg: NodeId,
        name: impl Into<String>,
        size: Bytes,
    ) -> Result<EditReceipt, EditError> {
        self.edit(
            expected_revision,
            PlanEdit::Add(PlanRequest::CreateLv {
                vg,
                name: name.into(),
                size,
                layout: LvmLvKind::Linear,
                activate: true,
            }),
        )
    }

    /// Adds a filesystem request without an initial label; unsupported kinds remain diagnosed.
    pub fn add_filesystem(
        &mut self,
        expected_revision: DraftRevision,
        backing: NodeId,
        filesystem: FilesystemKind,
    ) -> Result<EditReceipt, EditError> {
        self.edit(
            expected_revision,
            PlanEdit::Add(PlanRequest::CreateFilesystem {
                backing,
                filesystem,
                label: None,
            }),
        )
    }

    /// Adds a runtime mount with the entry's explicit options and namespace.
    pub fn mount(
        &mut self,
        expected_revision: DraftRevision,
        entry: MountEntry,
    ) -> Result<EditReceipt, EditError> {
        self.edit(expected_revision, PlanEdit::Add(PlanRequest::Mount(entry)))
    }

    /// Adds a persistent mount independently of the runtime mount request.
    pub fn persist_mount(
        &mut self,
        expected_revision: DraftRevision,
        entry: MountEntry,
    ) -> Result<EditReceipt, EditError> {
        self.edit(
            expected_revision,
            PlanEdit::Add(PlanRequest::PersistMount(entry)),
        )
    }

    /// Edits the original LV create size, preserving identities and never adding a resize action.
    pub fn set_created_lv_size(
        &mut self,
        expected_revision: DraftRevision,
        operation: OperationId,
        size: Bytes,
    ) -> Result<EditReceipt, EditError> {
        self.check_revision(expected_revision)?;
        let index = operation_index(&self.draft, operation)?;
        let mut request = self.draft.operations[index].request.clone();
        let PlanRequest::CreateLv {
            size: requested, ..
        } = &mut request
        else {
            return Err(EditError::NotCreatedLv(operation));
        };
        *requested = size;
        self.edit(expected_revision, PlanEdit::Replace { operation, request })
    }

    /// Changes planned partition placement without resizing or moving an existing partition.
    /// Invalid extents remain editable; stable operation and output identities are retained.
    pub fn set_created_partition_extent(
        &mut self,
        expected_revision: DraftRevision,
        operation: OperationId,
        offset: Bytes,
        size: Bytes,
    ) -> Result<EditReceipt, EditError> {
        self.check_revision(expected_revision)?;
        let index = operation_index(&self.draft, operation)?;
        let mut request = self.draft.operations[index].request.clone();
        let PlanRequest::CreatePartition {
            offset: requested_offset,
            size: requested_size,
            ..
        } = &mut request
        else {
            return Err(EditError::NotCreatedPartition(operation));
        };
        *requested_offset = offset;
        *requested_size = size;
        self.edit(expected_revision, PlanEdit::Replace { operation, request })
    }

    /// Returns this creator and its transitive existence dependents, in current request order.
    /// Shared resources and scheduling edges do not imply cancellation dependencies.
    pub fn removal_impact(&self, operation: OperationId) -> Result<Vec<OperationId>, EditError> {
        removal_impact(&self.draft, &[operation])
    }

    /// Publishes an explicitly reviewed rebase atomically, retaining operation/output IDs.
    /// Undo/redo restart at this baseline boundary; old frozen documents retain their own context.
    /// An invalid/blocked reviewed forecast remains editable and never grants write permission.
    pub fn accept_rebase(
        &mut self,
        expected_revision: DraftRevision,
        review: RebasePreview,
    ) -> Result<DraftRevision, RebaseError> {
        if expected_revision != self.revision {
            return Err(RebaseError::RevisionConflict {
                expected: expected_revision,
                actual: self.revision,
            });
        }
        if review.source.revision != self.revision {
            return Err(RebaseError::RevisionConflict {
                expected: review.source.revision,
                actual: self.revision,
            });
        }
        if review.source.session_id != self.id
            || review.source.baseline != *self.baseline
            || review.source.draft != self.draft
        {
            return Err(RebaseError::SourceContextChanged);
        }
        let revision = self.revision.next().ok_or(RebaseError::RevisionExhausted)?;
        self.baseline = Arc::new(review.baseline);
        self.draft = review.draft;
        self.preview = review.preview;
        self.revision = revision;
        self.undo.clear();
        self.redo.clear();
        Ok(revision)
    }

    /// Restores the previous draft and its identities; the publication revision still increases.
    pub fn undo(&mut self, expected_revision: DraftRevision) -> Result<DraftRevision, EditError> {
        let revision = self.check_revision(expected_revision)?;
        let candidate = self.undo.last().ok_or(EditError::NoUndo)?.clone();
        let preview = self.build_candidate(&candidate)?;
        let _ = self.undo.pop();
        push_history(
            &mut self.redo,
            std::mem::replace(&mut self.draft, candidate),
            self.history_limit,
        );
        self.preview = preview;
        self.revision = revision;
        Ok(revision)
    }

    /// Restores an undone draft and recomputes its preview without executing storage actions.
    pub fn redo(&mut self, expected_revision: DraftRevision) -> Result<DraftRevision, EditError> {
        let revision = self.check_revision(expected_revision)?;
        let candidate = self.redo.last().ok_or(EditError::NoRedo)?.clone();
        let preview = self.build_candidate(&candidate)?;
        let _ = self.redo.pop();
        push_history(
            &mut self.undo,
            std::mem::replace(&mut self.draft, candidate),
            self.history_limit,
        );
        self.preview = preview;
        self.revision = revision;
        Ok(revision)
    }

    /// Checks optimistic concurrency and overflow before any draft/history mutation.
    fn check_revision(&self, expected: DraftRevision) -> Result<DraftRevision, EditError> {
        if expected != self.revision {
            return Err(EditError::RevisionConflict {
                expected,
                actual: self.revision,
            });
        }
        self.revision.next().ok_or(EditError::RevisionExhausted)
    }

    /// User-plan failures remain editable; internal invariant failures abort publication.
    fn build_candidate(&self, candidate: &PlanDraft) -> Result<PreviewState, EditError> {
        let result = build_preview(&self.baseline, candidate);
        if let Err(error) = &result
            && error.is_internal()
        {
            return Err(EditError::InternalBuild(Box::new(error.clone())));
        }
        Ok(result.into())
    }
}

/// Retains the newest snapshots without growing the bounded history indefinitely.
fn push_history(history: &mut Vec<PlanDraft>, draft: PlanDraft, limit: usize) {
    if limit == 0 {
        return;
    }
    if history.len() >= limit {
        history.remove(0);
    }
    history.push(draft);
}

/// Resolves a stable operation identity before changing a candidate draft.
fn operation_index(draft: &PlanDraft, id: OperationId) -> Result<usize, EditError> {
    draft
        .operations
        .iter()
        .position(|operation| operation.id == id)
        .ok_or(EditError::UnknownOperation(id))
}

/// Finds all request identities whose existence depends on the selected operations.
fn removal_impact(
    draft: &PlanDraft,
    operations: &[OperationId],
) -> Result<Vec<OperationId>, EditError> {
    for operation in operations {
        operation_index(draft, *operation)?;
    }
    let selected: HashSet<_> = operations.iter().copied().collect();
    let mut affected = selected.clone();
    loop {
        let outputs: HashSet<_> = draft
            .operations
            .iter()
            .filter(|operation| affected.contains(&operation.id))
            .filter_map(|operation| operation.created_node)
            .collect();
        let mut changed = false;
        for operation in &draft.operations {
            if operation
                .request
                .referenced_nodes()
                .iter()
                .any(|node| outputs.contains(node))
            {
                changed |= affected.insert(operation.id);
            }
        }
        if !changed {
            break;
        }
    }
    Ok(draft
        .operations
        .iter()
        .filter(|operation| affected.contains(&operation.id))
        .map(|operation| operation.id)
        .collect())
}

/// Cancels only explicitly selected requests after computing all existence dependents.
fn remove_operations(draft: &mut PlanDraft, operations: &[OperationId]) -> Result<(), EditError> {
    let affected = removal_impact(draft, operations)?;
    let selected: HashSet<_> = operations.iter().copied().collect();
    let missing: Vec<_> = affected
        .into_iter()
        .filter(|operation| !selected.contains(operation))
        .collect();
    if !missing.is_empty() {
        return Err(EditError::DependentOperations {
            operations: missing,
        });
    }
    draft
        .operations
        .retain(|operation| !selected.contains(&operation.id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ProtectedMount,
        editing::{
            PreviewStatus,
            build::tests::{baseline, lv_request},
        },
    };
    use storage_core::model::{
        Bytes, DesiredState, FilesystemKind, MountContext, MountEntry, MountSource, OperationIntent,
    };
    use storage_provider::{LvmAction, ProviderAction};

    /// Applies a command at the current revision for sequential scenario tests.
    fn apply(session: &mut PlanSession, edit: PlanEdit) -> EditReceipt {
        session
            .edit(session.revision(), edit)
            .unwrap_or_else(|error| panic!("edit: {error}"))
    }

    /// Builds the full initial workflow through the public session API and allocated IDs.
    fn workflow_session() -> PlanSession {
        let mut session =
            PlanSession::new(baseline()).unwrap_or_else(|error| panic!("new session: {error}"));
        let lv = apply(&mut session, PlanEdit::Add(lv_request()))
            .created_node
            .unwrap_or_else(|| panic!("LV ID"));
        let filesystem = apply(
            &mut session,
            PlanEdit::Add(PlanRequest::CreateFilesystem {
                backing: lv,
                filesystem: FilesystemKind::Ext4,
                label: Some("data".to_owned()),
            }),
        )
        .created_node
        .unwrap_or_else(|| panic!("filesystem ID"));
        apply(
            &mut session,
            PlanEdit::Add(PlanRequest::Maintenance(OperationIntent::Check {
                target: filesystem,
                force: true,
            })),
        );
        let entry = MountEntry {
            source: MountSource::Filesystem(filesystem),
            target: "/data".into(),
            options: vec!["rw".to_owned()],
            context: MountContext::Host,
        };
        apply(
            &mut session,
            PlanEdit::Add(PlanRequest::Mount(entry.clone())),
        );
        apply(
            &mut session,
            PlanEdit::Add(PlanRequest::PersistMount(entry)),
        );
        session
    }

    /// Resize edits change the original create request, keeping all node IDs and baseline facts.
    #[test]
    fn workflow_edits_and_history_preserve_ids_and_conditional_evidence() {
        let mut session = workflow_session();
        let original = session.draft().clone();
        let initial_preview = session.preview().clone();
        assert_eq!(session.revision().get(), 5);
        assert_eq!(session.preview().status(), PreviewStatus::Conditional);
        let mut request = original.operations[0].request.clone();
        if let PlanRequest::CreateLv { size, .. } = &mut request {
            *size = Bytes::new(101 * 4 * 1024 * 1024);
        }
        let operation = original.operations[0].id;
        apply(&mut session, PlanEdit::Replace { operation, request });
        assert_eq!(session.preview().status(), PreviewStatus::Invalid);
        assert!(session.preview().calculated().is_none());
        assert_eq!(
            session.preview().diagnostics()[0].operation,
            Some(operation)
        );
        let invalid = session.draft().clone();
        assert_ne!(invalid, original);
        assert_eq!(
            invalid.operations[0].created_node,
            original.operations[0].created_node
        );
        let revision = session
            .undo(session.revision())
            .unwrap_or_else(|error| panic!("undo: {error}"));
        assert_eq!(revision.get(), 7);
        assert_eq!(session.draft(), &original);
        assert_eq!(session.preview(), &initial_preview);
        session
            .redo(session.revision())
            .unwrap_or_else(|error| panic!("redo: {error}"));
        assert_eq!(session.revision().get(), 8);
        assert_eq!(session.draft(), &invalid);
        assert_eq!(session.preview().status(), PreviewStatus::Invalid);
        let mut request = original.operations[0].request.clone();
        if let PlanRequest::CreateLv { size, .. } = &mut request {
            *size = Bytes::new(30 * 4 * 1024 * 1024);
        }
        apply(&mut session, PlanEdit::Replace { operation, request });
        let preview = session
            .preview()
            .calculated()
            .unwrap_or_else(|| panic!("repaired preview"));
        assert_eq!(preview.status(), PreviewStatus::Conditional);
        assert!(preview.plan.actions().iter().all(|action| !matches!(
            action.operation,
            ProviderAction::Lvm(LvmAction::ResizeLv { .. })
        )));
        for (old, new) in original.operations.iter().zip(&session.draft().operations) {
            assert_eq!(old.id, new.id);
            assert_eq!(old.created_node, new.created_node);
        }
        assert_eq!(session.baseline(), &baseline());
    }

    /// Explicit removal follows node existence, leaving an independent LV sharing the VG intact.
    #[test]
    fn creator_removal_requires_transitive_dependents_but_keeps_independent_requests() {
        let mut session = workflow_session();
        let original = session.draft().clone();
        let mut independent = lv_request();
        if let PlanRequest::CreateLv { name, .. } = &mut independent {
            *name = "logs".to_owned();
        }
        let independent = apply(&mut session, PlanEdit::Add(independent));
        let before = session.draft().clone();
        let preview = session.preview().clone();
        let revision = session.revision();
        let creator = original.operations[0].id;
        assert_eq!(
            session
                .removal_impact(creator)
                .unwrap_or_else(|error| panic!("impact: {error}")),
            original
                .operations
                .iter()
                .map(|operation| operation.id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            session.edit(
                revision,
                PlanEdit::Remove {
                    operations: vec![creator]
                }
            ),
            Err(EditError::DependentOperations {
                operations: original.operations[1..]
                    .iter()
                    .map(|operation| operation.id)
                    .collect()
            })
        );
        assert_eq!(session.draft(), &before);
        assert_eq!(session.preview(), &preview);
        assert_eq!(session.revision(), revision);
        apply(
            &mut session,
            PlanEdit::Remove {
                operations: original
                    .operations
                    .iter()
                    .map(|operation| operation.id)
                    .collect(),
            },
        );
        assert_eq!(session.draft().operations.len(), 1);
        assert_eq!(
            Some(session.draft().operations[0].id),
            independent.added_operation
        );
        assert_eq!(
            session.draft().operations[0].created_node,
            independent.created_node
        );
        assert!(session.preview().calculated().is_some());
        session
            .undo(session.revision())
            .unwrap_or_else(|error| panic!("undo removal: {error}"));
        assert_eq!(session.draft(), &before);
    }

    /// Invalid commands preserve all session state, including history available to undo/redo.
    #[test]
    fn rejected_commands_are_atomic_and_new_edits_clear_redo() {
        let mut session = workflow_session();
        let revision = session.revision();
        let draft = session.draft().clone();
        let preview = session.preview().clone();
        let undo = session.undo.clone();
        let redo = session.redo.clone();
        let unknown = OperationId::new();
        for edit in [
            PlanEdit::Replace {
                operation: unknown,
                request: lv_request(),
            },
            PlanEdit::Remove {
                operations: vec![unknown],
            },
            PlanEdit::MoveBefore {
                operation: draft.operations[0].id,
                before: Some(unknown),
            },
            PlanEdit::MoveBefore {
                operation: unknown,
                before: None,
            },
            PlanEdit::Replace {
                operation: draft.operations[0].id,
                request: PlanRequest::Unmount {
                    target: "/data".into(),
                    context: MountContext::Host,
                },
            },
        ] {
            assert!(session.edit(revision, edit).is_err());
            assert_eq!(session.draft(), &draft);
            assert_eq!(session.preview(), &preview);
            assert_eq!(session.revision(), revision);
            assert_eq!(session.undo, undo);
            assert_eq!(session.redo, redo);
        }
        assert_eq!(
            session.edit(DraftRevision::default(), PlanEdit::Clear),
            Err(EditError::RevisionConflict {
                expected: DraftRevision::default(),
                actual: revision
            })
        );
        assert_eq!(session.draft(), &draft);
        session
            .undo(revision)
            .unwrap_or_else(|error| panic!("undo: {error}"));
        assert!(matches!(
            session.redo(revision),
            Err(EditError::RevisionConflict { .. })
        ));
        apply(&mut session, PlanEdit::Clear);
        assert_eq!(session.redo(session.revision()), Err(EditError::NoRedo));
    }

    /// Reordering cannot bypass input creation and never retains an older successful future view.
    #[test]
    fn reorder_can_be_invalid_and_is_reversible() {
        let mut session = workflow_session();
        let original = session.draft().clone();
        let original_preview = session.preview().clone();
        apply(
            &mut session,
            PlanEdit::MoveBefore {
                operation: original.operations[1].id,
                before: Some(original.operations[0].id),
            },
        );
        assert_eq!(session.preview().status(), PreviewStatus::Invalid);
        assert!(session.preview().calculated().is_none());
        session
            .undo(session.revision())
            .unwrap_or_else(|error| panic!("undo move: {error}"));
        assert_eq!(session.draft(), &original);
        assert_eq!(session.preview(), &original_preview);
        apply(
            &mut session,
            PlanEdit::MoveBefore {
                operation: original.operations[3].id,
                before: None,
            },
        );
        assert_eq!(
            session
                .draft()
                .operations
                .last()
                .map(|operation| operation.id),
            Some(original.operations[3].id)
        );
        assert!(session.preview().calculated().is_some());
    }

    /// Clear returns to observed state without dropping protection; history restores both.
    #[test]
    fn protection_clear_and_empty_history_follow_revision_rules() {
        let mut session =
            PlanSession::new(baseline()).unwrap_or_else(|error| panic!("new: {error}"));
        assert_eq!(session.undo(session.revision()), Err(EditError::NoUndo));
        assert_eq!(session.redo(session.revision()), Err(EditError::NoRedo));
        let entry = MountEntry {
            source: MountSource::Tmpfs,
            target: "/data".into(),
            options: Vec::new(),
            context: MountContext::Host,
        };
        apply(&mut session, PlanEdit::Add(PlanRequest::Mount(entry)));
        let protected = ProtectedScope {
            runtime_mounts: vec![ProtectedMount {
                target: "/data".into(),
                context: MountContext::Host,
            }],
            ..ProtectedScope::default()
        };
        apply(&mut session, PlanEdit::SetProtection(protected.clone()));
        assert_eq!(session.preview().status(), PreviewStatus::Blocked);
        apply(&mut session, PlanEdit::Clear);
        assert_eq!(session.draft().protected_scope, protected);
        assert_eq!(
            session
                .preview()
                .calculated()
                .map(|preview| &preview.report.simulation.desired),
            Some(&DesiredState::from_current(&session.baseline().snapshot))
        );
        session
            .undo(session.revision())
            .unwrap_or_else(|error| panic!("undo clear: {error}"));
        assert_eq!(session.preview().status(), PreviewStatus::Blocked);
        session.revision = serde_json::from_value(serde_json::json!(u64::MAX))
            .unwrap_or_else(|error| panic!("max revision: {error}"));
        let draft = session.draft().clone();
        assert_eq!(
            session.edit(session.revision(), PlanEdit::Clear),
            Err(EditError::RevisionExhausted)
        );
        assert_eq!(session.draft(), &draft);
    }

    /// History keeps the nearest drafts, supports resizing, and can be disabled entirely.
    #[test]
    fn bounded_history_retains_nearest_undo_and_redo_states() {
        let mut session = workflow_session();
        session.set_history_limit(2);
        assert_eq!(session.history_limit(), 2);
        assert_eq!(session.undo.len(), 2);
        let latest = session.draft().clone();
        session
            .undo(session.revision())
            .unwrap_or_else(|error| panic!("undo: {error}"));
        session
            .undo(session.revision())
            .unwrap_or_else(|error| panic!("undo: {error}"));
        assert_eq!(session.undo(session.revision()), Err(EditError::NoUndo));
        assert_eq!(session.draft().operations.len(), 3);
        session
            .redo(session.revision())
            .unwrap_or_else(|error| panic!("redo: {error}"));
        session
            .redo(session.revision())
            .unwrap_or_else(|error| panic!("redo: {error}"));
        assert_eq!(session.draft(), &latest);
        let revision = session.revision();
        session.set_history_limit(0);
        assert_eq!(session.revision(), revision);
        assert_eq!(session.draft(), &latest);
        apply(&mut session, PlanEdit::Clear);
        assert!(session.undo.is_empty());
        assert!(session.redo.is_empty());
        assert_eq!(session.undo(session.revision()), Err(EditError::NoUndo));
        session.set_history_limit(1);
        apply(&mut session, PlanEdit::Add(lv_request()));
        apply(&mut session, PlanEdit::Clear);
        assert_eq!(session.undo.len(), 1);
        session
            .undo(session.revision())
            .unwrap_or_else(|error| panic!("undo: {error}"));
        assert_eq!(session.draft().operations.len(), 1);
        session.set_history_limit(0);
        assert_eq!(session.redo(session.revision()), Err(EditError::NoRedo));
    }

    /// The editor cannot grow a resumed maximum-size draft beyond the import/export bound.
    #[test]
    fn operation_limit_rejects_add_without_changing_state() {
        let mut document = PlanSession::new(baseline())
            .unwrap_or_else(|error| panic!("new: {error}"))
            .draft_document();
        let request = lv_request();
        document.draft.operations = (0..MAX_DRAFT_OPERATIONS)
            .map(|_| PlanOperation::new(request.clone()))
            .collect();
        let mut session = PlanSession::from_document(document)
            .unwrap_or_else(|error| panic!("resume maximum draft: {error}"));
        let draft = session.draft.clone();
        let preview = session.preview.clone();
        let revision = session.revision();
        assert_eq!(
            session.edit(revision, PlanEdit::Add(lv_request())),
            Err(EditError::OperationLimit)
        );
        assert_eq!(session.draft(), &draft);
        assert_eq!(session.preview(), &preview);
        assert_eq!(session.revision(), revision);
        assert!(session.undo.is_empty());
    }

    /// Size helpers reject stale, missing, and wrong-kind requests without touching history.
    #[test]
    fn convenience_edits_keep_concurrency_and_request_kind_checks() {
        let mut session = workflow_session();
        let revision = session.revision();
        let draft = session.draft().clone();
        let preview = session.preview().clone();
        let filesystem = draft.operations[1].id;
        let unknown = OperationId::new();
        assert_eq!(
            session.set_created_lv_size(revision, filesystem, Bytes::new(1024)),
            Err(EditError::NotCreatedLv(filesystem))
        );
        assert_eq!(
            session.set_created_lv_size(revision, unknown, Bytes::new(1024)),
            Err(EditError::UnknownOperation(unknown))
        );
        assert!(matches!(
            session.set_created_lv_size(DraftRevision::default(), unknown, Bytes::new(1024)),
            Err(EditError::RevisionConflict { .. })
        ));
        assert_eq!(
            session.removal_impact(unknown),
            Err(EditError::UnknownOperation(unknown))
        );
        assert_eq!(session.draft(), &draft);
        assert_eq!(session.preview(), &preview);
        assert_eq!(session.revision(), revision);
    }

    /// Accepted reviews atomically replace the baseline and establish a fresh undo boundary.
    #[test]
    fn accepted_rebase_keeps_ids_and_frozen_context_but_resets_history() {
        let (mut session, next, _, target) = crate::editing::rebase::tests::session_and_baseline();
        session.set_history_limit(2);
        let original = session.draft_document();
        let frozen = session
            .freeze(session.revision())
            .unwrap_or_else(|error| panic!("freeze: {error}"));
        let json = frozen
            .document()
            .canonical_json()
            .unwrap_or_else(|error| panic!("export: {error}"));
        let review = session
            .prepare_rebase(session.revision(), next.clone())
            .unwrap_or_else(|error| panic!("prepare: {error}"));
        let expected_draft = review.draft().clone();
        let expected_preview = review.preview().clone();
        let revision = session
            .accept_rebase(session.revision(), review)
            .unwrap_or_else(|error| panic!("accept: {error}"));
        assert_eq!(revision.get(), original.revision.get() + 1);
        assert_eq!(session.id(), original.session_id);
        assert_eq!(session.baseline(), &next);
        assert_eq!(session.draft(), &expected_draft);
        assert_eq!(session.preview(), &expected_preview);
        assert_eq!(session.history_limit(), 2);
        assert!(
            matches!(session.draft().operations[0].request, PlanRequest::CreateLv { vg, .. } if vg == target)
        );
        for (before, after) in original
            .draft
            .operations
            .iter()
            .zip(&session.draft().operations)
        {
            assert_eq!(before.id, after.id);
            assert_eq!(before.created_node, after.created_node);
        }
        assert_eq!(session.undo(revision), Err(EditError::NoUndo));
        assert_eq!(session.redo(revision), Err(EditError::NoRedo));
        assert_eq!(frozen.document().baseline, original.baseline);
        assert_eq!(
            frozen
                .document()
                .canonical_json()
                .unwrap_or_else(|error| panic!("old export: {error}")),
            json
        );
        session
            .set_created_lv_size(
                session.revision(),
                session.draft().operations[0].id,
                Bytes::new(25 * 4 * 1024 * 1024),
            )
            .unwrap_or_else(|error| panic!("new edit: {error}"));
        session
            .undo(session.revision())
            .unwrap_or_else(|error| panic!("undo new edit: {error}"));
        assert_eq!(session.draft(), &expected_draft);
        assert_eq!(session.baseline(), &next);
    }

    /// Stale, cross-session, and altered restored contexts cannot publish a reviewed candidate.
    #[test]
    fn rejected_rebase_acceptance_preserves_the_entire_session() {
        let (mut session, next, _, _) = crate::editing::rebase::tests::session_and_baseline();
        let review = session
            .prepare_rebase(session.revision(), next)
            .unwrap_or_else(|error| panic!("prepare: {error}"));
        let mut other = PlanSession::from_document(session.draft_document())
            .unwrap_or_else(|error| panic!("restore: {error}"));
        other.draft.operations[0].request = if let PlanRequest::CreateLv {
            vg,
            name,
            size,
            layout,
            activate: _,
        } = &other.draft.operations[0].request
        {
            PlanRequest::CreateLv {
                vg: *vg,
                name: name.clone(),
                size: *size,
                layout: layout.clone(),
                activate: false,
            }
        } else {
            panic!("LV request")
        };
        let altered = other.draft_document();
        assert_eq!(
            other.accept_rebase(other.revision(), review.clone()),
            Err(RebaseError::SourceContextChanged)
        );
        assert_eq!(other.draft_document(), altered);
        other.id = uuid::Uuid::now_v7();
        assert_eq!(
            other.accept_rebase(other.revision(), review.clone()),
            Err(RebaseError::SourceContextChanged)
        );
        apply(
            &mut session,
            PlanEdit::SetProtection(ProtectedScope::default()),
        );
        let saved = session.draft_document();
        let preview = session.preview.clone();
        let undo = session.undo.clone();
        let redo = session.redo.clone();
        assert!(matches!(
            session.accept_rebase(session.revision(), review),
            Err(RebaseError::RevisionConflict { .. })
        ));
        assert_eq!(session.draft_document(), saved);
        assert_eq!(session.preview, preview);
        assert_eq!(session.undo, undo);
        assert_eq!(session.redo, redo);
    }

    /// Accepting an invalid forecast preserves editability; overflow rejects before publication.
    #[test]
    fn accepting_invalid_rebase_and_revision_exhaustion_are_explicit() {
        let (mut session, mut next, _, _) = crate::editing::rebase::tests::session_and_baseline();
        next.capacity_evidence[0].available = storage_provider::AllocationQuantity::new(1);
        let review = session
            .prepare_rebase(session.revision(), next)
            .unwrap_or_else(|error| panic!("prepare: {error}"));
        session
            .accept_rebase(session.revision(), review)
            .unwrap_or_else(|error| panic!("accept invalid: {error}"));
        assert_eq!(session.preview().status(), PreviewStatus::Invalid);
        assert!(session.preview().calculated().is_none());
        assert!(session.freeze(session.revision()).is_err());
        let operation = session.draft().operations[0].id;
        session
            .set_created_lv_size(session.revision(), operation, Bytes::new(4 * 1024 * 1024))
            .unwrap_or_else(|error| panic!("repair request: {error}"));
        assert!(session.preview().calculated().is_some());
        session.revision = serde_json::from_value(serde_json::json!(u64::MAX))
            .unwrap_or_else(|error| panic!("max: {error}"));
        let mut next = session.baseline().clone();
        next.id = uuid::Uuid::now_v7();
        let review = session
            .prepare_rebase(session.revision(), next)
            .unwrap_or_else(|error| panic!("prepare at max: {error}"));
        let saved = session.draft_document();
        assert_eq!(
            session.accept_rebase(session.revision(), review),
            Err(RebaseError::RevisionExhausted)
        );
        assert_eq!(session.draft_document(), saved);
    }

    /// Malformed baseline evidence prevents opening a session instead of creating a broken editor.
    #[test]
    fn session_rejects_duplicate_baseline_capacity() {
        let mut baseline = baseline();
        baseline
            .capacity_evidence
            .push(baseline.capacity_evidence[0].clone());
        assert!(PlanSession::new(baseline).is_err());
    }
}
