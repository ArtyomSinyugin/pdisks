//! Current preview state and UI diagnostics; none of these states grants live admission.

use serde::Serialize;

use storage_core::model::DiagnosticSeverity;

use crate::{ActionId, ActionSimulationError, CompositionError, GoalStatus, PlanSimulationError};

use super::{
    DraftBuildError, OperationActions, OperationDiagnostic, OperationId, PlanPreview,
    lower::diagnostic,
};

/// Planning status computed from the current build rather than stored independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewStatus {
    /// No current DAG or predicted state could be calculated.
    Invalid,
    /// A forecast exists but violates protection or fails request coverage.
    Blocked,
    /// Planning succeeded with conditions that still require evidence.
    Conditional,
    /// All planning checks passed; live admission is still required before writes.
    Complete,
}

/// Result for the current draft only; an invalid edit never exposes an older desired state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewState {
    /// Calculated plan, predicted state, and qualifications.
    Calculated(Box<PlanPreview>),
    /// Current build failure with typed error and action ownership.
    Invalid(DraftBuildError),
}

impl From<Result<PlanPreview, DraftBuildError>> for PreviewState {
    /// Retains either the current forecast or its failure, never a stale successful forecast.
    fn from(result: Result<PlanPreview, DraftBuildError>) -> Self {
        match result {
            Ok(preview) => Self::Calculated(Box::new(preview)),
            Err(error) => Self::Invalid(error),
        }
    }
}

impl PlanPreview {
    /// Distinguishes blockers from missing evidence without weakening `is_complete()`.
    pub fn status(&self) -> PreviewStatus {
        if !self.report.protection_violations.is_empty()
            || self
                .request_coverage
                .iter()
                .any(|coverage| !coverage.covered)
            || self
                .report
                .intent_coverage
                .iter()
                .any(|coverage| !coverage.covered)
            || self
                .report
                .goal_checks
                .iter()
                .any(|check| matches!(check.status, GoalStatus::Violated { .. }))
            || self
                .diagnostics
                .iter()
                .any(|entry| entry.diagnostic.severity == DiagnosticSeverity::Error)
        {
            PreviewStatus::Blocked
        } else if self.report.is_complete() {
            PreviewStatus::Complete
        } else {
            PreviewStatus::Conditional
        }
    }
}

impl PreviewState {
    /// Current planning status, never execution authorization.
    pub fn status(&self) -> PreviewStatus {
        match self {
            Self::Calculated(preview) => preview.status(),
            Self::Invalid(_) => PreviewStatus::Invalid,
        }
    }

    /// Returns the current computed view, absent when this revision cannot be calculated.
    pub fn calculated(&self) -> Option<&PlanPreview> {
        match self {
            Self::Calculated(preview) => Some(preview),
            Self::Invalid(_) => None,
        }
    }

    /// Structured diagnostics with operation ownership and exact prerequisite checks.
    pub fn diagnostics(&self) -> Vec<OperationDiagnostic> {
        match self {
            Self::Invalid(error) => error.diagnostics(),
            Self::Calculated(preview) => {
                let mut diagnostics = preview.diagnostics.clone();
                for coverage in &preview.request_coverage {
                    diagnostics.extend(coverage.diagnostics.iter().cloned().map(|diagnostic| {
                        OperationDiagnostic {
                            operation: Some(coverage.operation),
                            diagnostic,
                            checks: Vec::new(),
                        }
                    }));
                }
                diagnostics
            }
        }
    }
}

/// Finds the stable request owner of a per-build action identity.
fn owner(source_map: &[OperationActions], action: ActionId) -> Option<OperationId> {
    source_map
        .iter()
        .find(|mapping| mapping.actions.contains(&action))
        .map(|mapping| mapping.operation)
}

impl DraftBuildError {
    /// Translates provenance into UI attribution while preserving the underlying typed error.
    pub fn diagnostics(&self) -> Vec<OperationDiagnostic> {
        let (operation, code, message, checks) = match self {
            Self::Draft { diagnostics } => return diagnostics.clone(),
            Self::Composition { source, source_map } => {
                let actions = match source.as_ref() {
                    CompositionError::ActionValidation {
                        action,
                        diagnostics,
                    } => {
                        return diagnostics
                            .iter()
                            .cloned()
                            .map(|diagnostic| OperationDiagnostic {
                                operation: owner(source_map, *action),
                                diagnostic,
                                checks: Vec::new(),
                            })
                            .collect();
                    }
                    CompositionError::ActionRejected { action, checks } => {
                        return vec![OperationDiagnostic {
                            operation: owner(source_map, *action),
                            diagnostic: diagnostic("draft.prerequisites", source.to_string()),
                            checks: checks.clone(),
                        }];
                    }
                    CompositionError::ProtectedScopeViolation { action, .. }
                    | CompositionError::UnresolvedInput { action, .. }
                    | CompositionError::Effect { action, .. }
                    | CompositionError::UnresolvedRequirement { action }
                    | CompositionError::AmbiguousRequirement { action }
                    | CompositionError::RequirementNotInContract { action }
                    | CompositionError::MissingRequirementCheck(action)
                    | CompositionError::MissingAnalysedContract(action)
                    | CompositionError::SelfDependency(action) => vec![*action],
                    CompositionError::DuplicatePlannedNode { first, second, .. }
                    | CompositionError::AmbiguousResourceOrder { first, second, .. } => {
                        vec![*first, *second]
                    }
                    _ => Vec::new(),
                };
                if !actions.is_empty() {
                    return actions
                        .into_iter()
                        .map(|action| OperationDiagnostic {
                            operation: owner(source_map, action),
                            diagnostic: diagnostic("draft.composition", source.to_string()),
                            checks: Vec::new(),
                        })
                        .collect();
                }
                (None, "draft.composition", source.to_string(), Vec::new())
            }
            Self::Simulation { source, source_map } => match source.as_ref() {
                PlanSimulationError::Action { action, source } => match source {
                    ActionSimulationError::Validation { diagnostics } => {
                        return diagnostics
                            .iter()
                            .cloned()
                            .map(|diagnostic| OperationDiagnostic {
                                operation: owner(source_map, *action),
                                diagnostic,
                                checks: Vec::new(),
                            })
                            .collect();
                    }
                    ActionSimulationError::PrerequisitesViolated { checks } => (
                        owner(source_map, *action),
                        "draft.prerequisites",
                        source.to_string(),
                        checks.clone(),
                    ),
                    ActionSimulationError::Effect(_) => (
                        owner(source_map, *action),
                        "draft.simulation",
                        source.to_string(),
                        Vec::new(),
                    ),
                },
                _ => (None, "draft.simulation", source.to_string(), Vec::new()),
            },
        };
        vec![OperationDiagnostic {
            operation,
            diagnostic: diagnostic(code, message),
            checks,
        }]
    }

    /// Internal contract/identity failures must abort a session edit instead of publishing it.
    pub fn is_internal(&self) -> bool {
        matches!(self,
            Self::Composition { source, .. } if matches!(source.as_ref(),
                CompositionError::MissingRequirementCheck(_) | CompositionError::MissingAnalysedContract(_)
                | CompositionError::RequirementNotInContract { .. } | CompositionError::UnknownLocalDependency(_)
                | CompositionError::UnknownRequirementAction { .. } | CompositionError::DuplicateLocalAction { .. }
                | CompositionError::UnknownOrderingAction(_)))
            || matches!(self, Self::Simulation { source, .. } if matches!(source.as_ref(),
                PlanSimulationError::ContractMismatch(_) | PlanSimulationError::UnknownIntentOrigin { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editing::{
        PlanDraft, PlanOperation,
        build::tests::{baseline, lv_request, workflow},
        build_preview,
    };
    use crate::{CapacityEvidenceError, ProtectedMount};

    /// Empty, conditional, protected, and uncovered forecasts have distinct planning statuses.
    #[test]
    fn status_distinguishes_complete_conditional_and_blocked() {
        let baseline = baseline();
        let empty = build_preview(&baseline, &PlanDraft::default())
            .unwrap_or_else(|error| panic!("empty: {error}"));
        assert_eq!(empty.status(), PreviewStatus::Complete);
        let mut draft = workflow();
        let preview =
            build_preview(&baseline, &draft).unwrap_or_else(|error| panic!("workflow: {error}"));
        assert_eq!(preview.status(), PreviewStatus::Conditional);
        let mut uncovered = preview.clone();
        uncovered.request_coverage[0].covered = false;
        assert_eq!(uncovered.status(), PreviewStatus::Blocked);
        draft.protected_scope.runtime_mounts.push(ProtectedMount {
            target: "/data".into(),
            context: storage_core::model::MountContext::Host,
        });
        let protected = build_preview(&baseline, &draft)
            .unwrap_or_else(|error| panic!("protected preview: {error}"));
        assert_eq!(protected.status(), PreviewStatus::Blocked);
        assert!(!protected.report.protection_violations.is_empty());
        let result: PreviewState = Ok(preview).into();
        assert!(result.calculated().is_some());
    }

    /// Capacity failures preserve their exact checks and point to the newly rejected request.
    #[test]
    fn invalid_preview_has_no_desired_state_and_maps_prerequisites() {
        let baseline = baseline();
        let mut request = lv_request();
        if let super::super::PlanRequest::CreateLv { size, .. } = &mut request {
            *size = storage_core::model::Bytes::new(101 * 4 * 1024 * 1024);
        }
        let operation = PlanOperation::new(request);
        let id = operation.id;
        let draft = PlanDraft {
            operations: vec![operation],
            ..PlanDraft::default()
        };
        let state: PreviewState = build_preview(&baseline, &draft).into();
        assert_eq!(state.status(), PreviewStatus::Invalid);
        assert!(state.calculated().is_none());
        let diagnostics = state.diagnostics();
        assert_eq!(diagnostics[0].operation, Some(id));
        assert!(
            diagnostics[0]
                .checks
                .iter()
                .any(|check| matches!(check.status, crate::CheckStatus::Violated { .. }))
        );
    }

    /// Programming errors differ from rejected user plans and baseline evidence errors.
    #[test]
    fn internal_failures_are_not_user_plan_rejections() {
        let internal = DraftBuildError::Composition {
            source: Box::new(CompositionError::MissingAnalysedContract(ActionId::new(0))),
            source_map: Vec::new(),
        };
        assert!(internal.is_internal());
        let external = DraftBuildError::Composition {
            source: Box::new(CompositionError::CapacityEvidence(
                CapacityEvidenceError::DuplicateDomain(
                    storage_provider::AllocationDomain::LvmVolumeGroup(
                        storage_core::model::NodeId::new(),
                    ),
                ),
            )),
            source_map: Vec::new(),
        };
        assert!(!external.is_internal());
    }
}
