//! Rebuilds a user draft against one historical baseline through the existing engine.

use std::collections::HashSet;

use storage_core::model::{Diagnostic, OperationIntent};
use storage_provider::{DependencyReason, PlanFragment};
use thiserror::Error;

use crate::{
    ActionId, CompositionError, ExecutionPlan, FragmentActionRef, FragmentId, FragmentOrdering,
    HistoricalBaseline, PlanSimulationError, PrerequisiteCheck, SimulationReport, analyse_plan,
    compose_plan_with_ordering,
};

use super::{OperationId, PlanDraft, PlanRequest, lower::diagnostic, lower_operation};

/// Per-build links between a stable user request and its generated actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationActions {
    /// Stable identity retained across draft revisions.
    pub operation: OperationId,
    /// Positions of this request's fragments in this build.
    pub fragments: Vec<FragmentId>,
    /// Global action IDs assigned by the same helper used by composition.
    pub actions: Vec<ActionId>,
}

/// Existing storage diagnostic attributed to the request that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationDiagnostic {
    /// Owning request, or no request for a baseline or whole-plan issue.
    pub operation: Option<OperationId>,
    /// Structured provider or draft diagnostic.
    pub diagnostic: Diagnostic,
    /// Full prerequisite checks when transition validation rejected an action.
    pub checks: Vec<PrerequisiteCheck>,
}

/// Current calculated forecast; it never supplies permission for real writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPreview {
    /// Immutable validated action DAG for this draft build.
    pub plan: ExecutionPlan,
    /// Predicted state and every qualification from the shared simulator.
    pub report: SimulationReport,
    /// Action ownership, retained separately from human-readable origin descriptions.
    pub source_map: Vec<OperationActions>,
    /// Nonfatal diagnostics produced while lowering requests.
    pub diagnostics: Vec<OperationDiagnostic>,
}

/// A rejected draft retains its original typed storage error and action ownership.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DraftBuildError {
    /// Invalid identities, unresolved inputs, or unsupported requests.
    #[error("draft validation or lowering failed")]
    Draft {
        /// Issues attributed to requests without parsing display strings.
        diagnostics: Vec<OperationDiagnostic>,
    },
    /// The existing composer rejected structure, prerequisites, or resource ordering.
    #[error("draft composition failed: {source}")]
    Composition {
        /// Complete engine error, including exact prerequisite results.
        source: Box<CompositionError>,
        /// Ownership assigned before the failed composition.
        source_map: Vec<OperationActions>,
    },
    /// Full replay rejected the candidate plan or its protection policy.
    #[error("draft analysis failed: {source}")]
    Simulation {
        /// Existing simulation error, without a parallel error catalogue.
        source: Box<PlanSimulationError>,
        /// Ownership of all actions in the attempted build.
        source_map: Vec<OperationActions>,
    },
}

/// Computes a fresh forecast without modifying the baseline or retaining a second future graph.
pub fn build_preview(
    baseline: &HistoricalBaseline,
    draft: &PlanDraft,
) -> Result<PlanPreview, DraftBuildError> {
    validate_draft(baseline, draft)?;
    let mut intents: Vec<OperationIntent> = Vec::new();
    let mut fragments = Vec::new();
    let mut diagnostics = Vec::new();
    for operation in &draft.operations {
        let index = if let PlanRequest::Maintenance(intent) = &operation.request {
            intents.push(intent.clone());
            Some(intents.len() - 1)
        } else {
            None
        };
        let fragment =
            lower_operation(operation, index).map_err(|errors| DraftBuildError::Draft {
                diagnostics: errors
                    .into_iter()
                    .map(|diagnostic| OperationDiagnostic {
                        operation: Some(operation.id),
                        diagnostic,
                        checks: Vec::new(),
                    })
                    .collect(),
            })?;
        diagnostics.extend(fragment.diagnostics.iter().cloned().map(|diagnostic| {
            OperationDiagnostic {
                operation: Some(operation.id),
                diagnostic,
                checks: Vec::new(),
            }
        }));
        fragments.push(fragment);
    }
    build_fragments(baseline, draft, fragments, &intents, diagnostics)
}

/// Composes lowered fragments with exact input identities, retaining ownership on failure.
fn build_fragments(
    baseline: &HistoricalBaseline,
    draft: &PlanDraft,
    fragments: Vec<PlanFragment>,
    intents: &[OperationIntent],
    diagnostics: Vec<OperationDiagnostic>,
) -> Result<PlanPreview, DraftBuildError> {
    let ids = crate::compose::fragment_action_ids(&fragments).map_err(|source| {
        DraftBuildError::Composition {
            source: Box::new(source),
            source_map: Vec::new(),
        }
    })?;
    let source_map: Vec<_> = draft
        .operations
        .iter()
        .zip(&fragments)
        .enumerate()
        .map(|(index, (operation, fragment))| {
            // The shared allocator has already checked that every fragment index fits u32.
            let fragment_id = FragmentId::new(index as u32);
            OperationActions {
                operation: operation.id,
                fragments: vec![fragment_id],
                actions: fragment
                    .actions
                    .iter()
                    .map(|action| ids[&(index, action.id)])
                    .collect(),
            }
        })
        .collect();
    let mut ordering = Vec::new();
    let mut previous_sinks = Vec::new();
    for (index, fragment) in fragments.iter().enumerate() {
        if fragment.actions.is_empty() {
            continue;
        }
        let endpoint = |action| FragmentActionRef {
            fragment: FragmentId::new(index as u32),
            action,
        };
        let roots: Vec<_> = fragment
            .actions
            .iter()
            .filter(|action| {
                !fragment
                    .dependencies
                    .iter()
                    .any(|edge| edge.after == action.id)
            })
            .map(|action| endpoint(action.id))
            .collect();
        for before in &previous_sinks {
            for after in &roots {
                ordering.push(FragmentOrdering {
                    before: *before,
                    after: *after,
                    reason: DependencyReason::RequestedOrder,
                });
            }
        }
        previous_sinks = fragment
            .actions
            .iter()
            .filter(|action| {
                !fragment
                    .dependencies
                    .iter()
                    .any(|edge| edge.before == action.id)
            })
            .map(|action| endpoint(action.id))
            .collect();
    }
    let plan = compose_plan_with_ordering(
        &baseline.snapshot,
        fragments,
        &ordering,
        &baseline.capacity_evidence,
    )
    .map_err(|source| DraftBuildError::Composition {
        source: Box::new(source),
        source_map: source_map.clone(),
    })?;
    let report = analyse_plan(
        &baseline.snapshot,
        &plan,
        intents,
        &baseline.capacity_evidence,
        &draft.protected_scope,
    )
    .map_err(|source| DraftBuildError::Simulation {
        source: Box::new(source),
        source_map: source_map.clone(),
    })?;
    Ok(PlanPreview {
        plan,
        report,
        source_map,
        diagnostics,
    })
}

/// Validates untrusted draft identity and input references before lowering can allocate actions.
fn validate_draft(baseline: &HistoricalBaseline, draft: &PlanDraft) -> Result<(), DraftBuildError> {
    let mut diagnostics = Vec::new();
    let mut issue = |operation, code, message| {
        diagnostics.push(OperationDiagnostic {
            operation,
            diagnostic: diagnostic(code, message),
            checks: Vec::new(),
        })
    };
    if baseline.id.is_nil() {
        issue(
            None,
            "draft.baseline_identity",
            "baseline identity must not be nil",
        );
    }
    let mut operations = HashSet::new();
    let mut outputs = HashSet::new();
    for operation in &draft.operations {
        if operation.id.as_uuid().is_nil() || !operations.insert(operation.id) {
            issue(
                Some(operation.id),
                "draft.operation_identity",
                "operation identities must be non-nil and unique",
            );
        }
        if operation.request.creates_node() != operation.created_node.is_some() {
            issue(
                Some(operation.id),
                "draft.created_node",
                "output identity does not match the request kind",
            );
        }
        if let Some(node) = operation.created_node
            && (node.as_uuid().is_nil()
                || baseline.snapshot.graph.node(&node).is_some()
                || !outputs.insert(node))
        {
            issue(
                Some(operation.id),
                "draft.output_identity",
                "planned nodes must be non-nil, unique, and absent from the baseline",
            );
        }
    }
    for operation in &draft.operations {
        for node in operation.request.referenced_nodes() {
            if baseline.snapshot.graph.node(&node).is_none() && !outputs.contains(&node) {
                issue(
                    Some(operation.id),
                    "draft.unresolved_input",
                    "request input is absent from the baseline and draft outputs",
                );
            }
        }
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(DraftBuildError::Draft { diagnostics })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::editing::PlanOperation;
    use storage_core::model::{
        Bytes, DesiredState, FilesystemKind, LvmLvKind, MountContext, MountEntry, MountSource,
        NodeId,
    };
    use storage_provider::{AllocationDomain, LvmAction, ProviderAction};

    /// Loads the same immutable historical context as the offline integration tests.
    pub(super) fn baseline() -> HistoricalBaseline {
        serde_json::from_str(include_str!("../../tests/fixtures/editing_baseline.json"))
            .unwrap_or_else(|error| panic!("decode baseline: {error}"))
    }

    /// Creates one active linear volume request within the fixture capacity.
    pub(super) fn lv_request() -> PlanRequest {
        let AllocationDomain::LvmVolumeGroup(vg) = baseline().capacity_evidence[0].domain;
        PlanRequest::CreateLv {
            vg,
            name: "data".to_owned(),
            size: Bytes::new(20 * 4 * 1024 * 1024),
            layout: LvmLvKind::Linear,
            activate: true,
        }
    }

    /// Assembles LV, filesystem, check, and independent runtime/persistent mount requests.
    pub(super) fn workflow() -> PlanDraft {
        let lv = PlanOperation::new(lv_request());
        let node = lv.created_node.unwrap_or_else(|| panic!("LV output"));
        let filesystem = PlanOperation::new(PlanRequest::CreateFilesystem {
            backing: node,
            filesystem: FilesystemKind::Ext4,
            label: Some("data".to_owned()),
        });
        let node = filesystem
            .created_node
            .unwrap_or_else(|| panic!("filesystem output"));
        let entry = MountEntry {
            source: MountSource::Filesystem(node),
            target: "/data".into(),
            options: vec!["rw".to_owned(), "nosuid".to_owned()],
            context: MountContext::Host,
        };
        PlanDraft {
            operations: vec![
                lv,
                filesystem,
                PlanOperation::new(PlanRequest::Maintenance(OperationIntent::Check {
                    target: node,
                    force: true,
                })),
                PlanOperation::new(PlanRequest::Mount(entry.clone())),
                PlanOperation::new(PlanRequest::PersistMount(entry)),
            ],
            ..PlanDraft::default()
        }
    }

    /// Empty and populated previews replay from the baseline and retain stable action ownership.
    #[test]
    fn preview_rebuilds_from_baseline_with_source_map() {
        let baseline = baseline();
        let before = baseline.clone();
        let empty = build_preview(&baseline, &PlanDraft::default())
            .unwrap_or_else(|error| panic!("empty preview: {error}"));
        assert_eq!(
            empty.report.simulation.desired,
            DesiredState::from_current(&baseline.snapshot)
        );
        let draft = workflow();
        let preview = build_preview(&baseline, &draft)
            .unwrap_or_else(|error| panic!("workflow preview: {error}"));
        assert_eq!(preview.plan.actions().len(), 6);
        assert_eq!(preview.source_map.len(), 5);
        for (operation, mapping) in draft.operations.iter().zip(&preview.source_map) {
            assert_eq!(operation.id, mapping.operation);
            for action in &mapping.actions {
                assert!(
                    mapping
                        .fragments
                        .contains(&preview.plan.actions()[action.get() as usize].fragment)
                );
            }
        }
        assert_eq!(preview.report.simulation.desired.intents.len(), 1);
        assert!(preview.report.intent_coverage[0].covered);
        assert_eq!(preview.report.simulation.desired.mounts.runtime.len(), 1);
        assert_eq!(preview.report.simulation.desired.mounts.persistent.len(), 1);
        assert!(!preview.report.is_complete());
        assert_eq!(build_preview(&baseline, &draft), Ok(preview));
        assert_eq!(baseline, before);
    }

    /// User order must not reverse mandatory creation; failed builds retain action ownership.
    #[test]
    fn preview_rejects_reversed_creation_and_cumulative_overallocation() {
        let baseline = baseline();
        let mut draft = workflow();
        draft.operations.swap(0, 1);
        assert!(
            matches!(build_preview(&baseline, &draft), Err(DraftBuildError::Composition { source, .. }) if *source == CompositionError::DependencyCycle)
        );
        let mut draft = PlanDraft::default();
        for name in ["data", "logs"] {
            let mut request = lv_request();
            if let PlanRequest::CreateLv {
                name: lv_name,
                size,
                ..
            } = &mut request
            {
                *lv_name = name.to_owned();
                *size = Bytes::new(60 * 4 * 1024 * 1024);
            }
            draft.operations.push(PlanOperation::new(request));
        }
        let Err(DraftBuildError::Composition { source, source_map }) =
            build_preview(&baseline, &draft)
        else {
            panic!("over-allocation must fail")
        };
        let CompositionError::ActionRejected { action, checks } = *source else {
            panic!("expected prerequisite rejection")
        };
        assert_eq!(action, ActionId::new(2));
        assert_eq!(source_map[1].operation, draft.operations[1].id);
        assert!(source_map[1].actions.contains(&action));
        assert!(
            checks
                .iter()
                .any(|check| matches!(check.status, crate::CheckStatus::Violated { .. }))
        );
    }

    /// Public/imported drafts cannot smuggle duplicate, nil, colliding, or missing identities.
    #[test]
    fn imported_drafts_validate_identities_and_references() {
        let baseline = baseline();
        let original = workflow();
        for case in 0..7 {
            let mut draft = original.clone();
            match case {
                0 => draft.operations[1].id = draft.operations[0].id,
                1 => draft.operations[1].created_node = draft.operations[0].created_node,
                2 => draft.operations[0].created_node = None,
                3 => draft.operations[2].created_node = Some(NodeId::new()),
                4 => draft.operations[0].created_node = Some(NodeId::from_uuid(uuid::Uuid::nil())),
                5 => {
                    draft.operations[0].id =
                        serde_json::from_value(serde_json::json!(uuid::Uuid::nil()))
                            .unwrap_or_else(|error| panic!("nil ID: {error}"))
                }
                _ => {
                    draft.operations[1].request = PlanRequest::CreateFilesystem {
                        backing: NodeId::new(),
                        filesystem: FilesystemKind::Ext4,
                        label: None,
                    }
                }
            }
            assert!(
                matches!(build_preview(&baseline, &draft), Err(DraftBuildError::Draft { diagnostics }) if !diagnostics.is_empty())
            );
        }
        let preview =
            build_preview(&baseline, &original).unwrap_or_else(|error| panic!("original: {error}"));
        assert!(matches!(
            &preview.plan.actions()[0].operation,
            ProviderAction::Lvm(LvmAction::CreateLv { .. })
        ));
    }
}
