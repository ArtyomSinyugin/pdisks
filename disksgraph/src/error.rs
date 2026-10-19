//! Stable application error categories; human messages never determine control flow.

use serde::Serialize;
use storage_engine::{
    ExecutionRunError, PlanDocumentError,
    editing::{DraftBuildError, DraftDocumentError, EditError, FreezeError, RebaseError},
};
use storage_runtime::{
    checkpoint::CheckpointError, coordinator::CoordinatorError, host::HostError,
    incident::IncidentError, lease::LeaseError,
};

/// Machine-readable application errors, shared by future CLI and ABI adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Handle is not owned by this application instance.
    UnknownSession,
    /// Immutable plan handle is absent from this instance.
    UnknownPlan,
    /// Execution attempt handle is absent from this instance.
    UnknownRun,
    /// Bounded collection or request limit was reached before any effect.
    ResourceLimit,
    /// An optimistic edit or freeze addressed another revision.
    RevisionConflict,
    /// Editing command was rejected atomically.
    EditRejected,
    /// Saved editable inputs failed validation.
    InvalidDraft,
    /// Frozen plan schema or trusted contract reconstruction failed.
    InvalidPlan,
    /// Preview is invalid or blocked and cannot be frozen.
    CannotFreeze,
    /// Fresh identity correlation or rebase validation failed.
    RebaseRejected,
    /// Actual provider state could not be collected.
    ProbeFailed,
    /// Host exclusion or private-directory checks failed.
    LeaseUnavailable,
    /// A checkpoint is pending, corrupt, or could not be published/removed safely.
    CheckpointBlocked,
    /// Whole-plan native readiness did not pass.
    PlanUnavailable,
    /// Live admission was rejected or cannot be supplied safely.
    AdmissionRejected,
    /// A previously started attempt cannot be continued automatically.
    ExecutionStopped,
    /// Native receipt or fresh actual result did not establish the promised effect.
    VerificationFailed,
    /// Event cursor refers to updates this run has not published.
    InvalidEventCursor,
    /// Run cannot publish another monotonic event.
    EventSequenceExhausted,
    /// Reviewed incident changed or lacks explicit safe acknowledgment.
    IncidentRejected,
}

/// Serializable diagnostic with a stable code; it contains no replayable authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct DisksGraphError {
    /// Stable category for callers; never parse message text to choose behavior.
    pub code: ErrorCode,
    /// Human-readable context copied from the typed failure.
    pub message: String,
    /// New attempt retained by a failed apply call; other errors carry no newly created run.
    pub run_id: Option<crate::RunId>,
}
impl DisksGraphError {
    /// Constructs an application diagnostic at an already classified trust boundary.
    pub(crate) fn new(code: ErrorCode, error: impl std::fmt::Display) -> Self {
        Self {
            code,
            message: error.to_string(),
            run_id: None,
        }
    }
}
impl From<ExecutionRunError> for DisksGraphError {
    /// Retains distinct cursor/exhaustion failures without inspecting display strings.
    fn from(error: ExecutionRunError) -> Self {
        let code = match &error {
            ExecutionRunError::InvalidEventCursor { .. } => ErrorCode::InvalidEventCursor,
            ExecutionRunError::EventSequenceExhausted => ErrorCode::EventSequenceExhausted,
            ExecutionRunError::Document(_) => ErrorCode::InvalidPlan,
            _ => ErrorCode::ExecutionStopped,
        };
        Self::new(code, error)
    }
}
impl From<EditError> for DisksGraphError {
    /// Editing conflicts remain distinguishable from rejected commands.
    fn from(error: EditError) -> Self {
        let code = if matches!(error, EditError::RevisionConflict { .. }) {
            ErrorCode::RevisionConflict
        } else {
            ErrorCode::EditRejected
        };
        Self::new(code, error)
    }
}
impl From<FreezeError> for DisksGraphError {
    /// Freezing invalid forecasts cannot be mistaken for authorization failure.
    fn from(error: FreezeError) -> Self {
        let code = match &error {
            FreezeError::RevisionConflict { .. } => ErrorCode::RevisionConflict,
            FreezeError::Document(_) => ErrorCode::InvalidPlan,
            _ => ErrorCode::CannotFreeze,
        };
        Self::new(code, error)
    }
}
impl From<RebaseError> for DisksGraphError {
    /// Failed correlation and stale editing revisions remain explicit.
    fn from(error: RebaseError) -> Self {
        let code = if matches!(error, RebaseError::RevisionConflict { .. }) {
            ErrorCode::RevisionConflict
        } else {
            ErrorCode::RebaseRejected
        };
        Self::new(code, error)
    }
}
impl From<DraftDocumentError> for DisksGraphError {
    /// Imported drafts are data, never trusted execution state.
    fn from(error: DraftDocumentError) -> Self {
        Self::new(ErrorCode::InvalidDraft, error)
    }
}
impl From<DraftBuildError> for DisksGraphError {
    /// Failure to create an editing context supplies no session handle.
    fn from(error: DraftBuildError) -> Self {
        Self::new(ErrorCode::InvalidDraft, error)
    }
}
impl From<PlanDocumentError> for DisksGraphError {
    /// Historical validation never supplies permission to dispatch.
    fn from(error: PlanDocumentError) -> Self {
        Self::new(ErrorCode::InvalidPlan, error)
    }
}
impl From<LeaseError> for DisksGraphError {
    /// OS exclusion is an application-visible prerequisite.
    fn from(error: LeaseError) -> Self {
        Self::new(ErrorCode::LeaseUnavailable, error)
    }
}
impl From<CheckpointError> for DisksGraphError {
    /// Unsafe or unreadable markers cannot be treated as absent.
    fn from(error: CheckpointError) -> Self {
        Self::new(ErrorCode::CheckpointBlocked, error)
    }
}
impl From<HostError> for DisksGraphError {
    /// Classifies runtime boundaries without turning unknown evidence into a known contradiction.
    fn from(error: HostError) -> Self {
        match error {
            HostError::Run(error) => error.into(),
            HostError::Document(error) => error.into(),
            HostError::Checkpoint(error) => error.into(),
            HostError::Probe(error) => Self::new(ErrorCode::ProbeFailed, error),
            HostError::Binding(error) => Self::new(ErrorCode::VerificationFailed, error),
            HostError::ActiveAttempt => Self::new(ErrorCode::CheckpointBlocked, error),
            _ => Self::new(ErrorCode::AdmissionRejected, error),
        }
    }
}
impl From<CoordinatorError> for DisksGraphError {
    /// The facade preserves the existing coordinator's failure boundary and diagnostic class.
    fn from(error: CoordinatorError) -> Self {
        match error {
            CoordinatorError::Host(error) => error.into(),
            CoordinatorError::Lease(error) => error.into(),
            CoordinatorError::Checkpoint(error) => error.into(),
            CoordinatorError::Run(error) => error.into(),
            CoordinatorError::Binding(error) => Self::new(ErrorCode::VerificationFailed, error),
            CoordinatorError::Receipt(error) => Self::new(ErrorCode::VerificationFailed, error),
            CoordinatorError::PlanUnavailable(_) => Self::new(ErrorCode::PlanUnavailable, error),
            _ => Self::new(ErrorCode::ExecutionStopped, error),
        }
    }
}
impl From<IncidentError> for DisksGraphError {
    /// Incident review failure never authorizes clear or retry.
    fn from(error: IncidentError) -> Self {
        Self::new(ErrorCode::IncidentRejected, error)
    }
}
