#![forbid(unsafe_code)]

//! Planning, validation, and simulation over trusted provider contracts.

mod compose;
pub mod editing;
mod effects;
mod execution;
pub mod journal;
pub mod live;
mod observe;
mod plan;
mod prerequisites;
mod simulate;
mod simulation_evidence;
mod validate;
mod wire;

pub use compose::{
    CompositionError, compose_plan, compose_plan_for_goal, compose_plan_for_goal_with_evidence,
    compose_plan_for_goal_with_policy, compose_plan_with_evidence, compose_plan_with_ordering,
};
pub use plan::{
    ActionDependency, ActionId, ExecutionPlan, FragmentActionRef, FragmentId, FragmentOrdering,
    PlannedAction,
};
pub use simulate::{
    GoalAspect, GoalCheck, GoalStatus, IntentCoverage, PlanSimulationError, SimulatedStep,
    SimulationObligation, SimulationReport, SimulationResult, analyse_plan, compare_goal,
    simulate_plan, simulate_plan_for_goal, simulate_plan_for_goal_with_evidence,
    simulate_plan_for_goal_with_policy, simulate_plan_with_evidence,
};
pub use simulation_evidence::{
    ActionSimulationError, CapacityEvidence, CapacityEvidenceError, CheckEvidence, CheckStatus,
    EffectApplicationError, PrerequisiteCheck, SimulationEvidenceSummary,
};

pub use validate::{ProtectedAspect, ProtectedMount, ProtectedScope, ProtectionViolation};

pub use wire::{
    HistoricalAnalysis, HistoricalBaseline, PLAN_SCHEMA_VERSION, PlanDocument, PlanDocumentError,
};

pub use execution::{
    ActionRunState, AdmissionReport, BackendCompletion, BackendOutcome, ExecutionContext,
    ExecutionRun, ExecutionRunError, GoalVerificationReport, LiveCheck, LiveCheckStatus,
    LiveExecutionChecks, LiveObservation, ObservationReference, RunEvent, RunEventKind,
    VerificationReport,
};
