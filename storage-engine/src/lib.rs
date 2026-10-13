#![forbid(unsafe_code)]

//! Planning, validation, and simulation over trusted provider contracts.

mod compose;
mod plan;
mod projection;
mod simulate;
mod validate;

pub use compose::{
    CompositionError, compose_plan, compose_plan_for_goal, compose_plan_for_goal_with_evidence,
    compose_plan_for_goal_with_policy, compose_plan_with_evidence,
};
pub use plan::{ActionDependency, ActionId, ExecutionPlan, FragmentId, PlannedAction};
pub use projection::{
    ActionSimulationError, CapacityEvidence, CapacityEvidenceError, CheckEvidence, CheckStatus,
    EffectApplicationError, PrerequisiteCheck, Projection, SimulatedAction,
};
pub use simulate::{
    GoalAspect, GoalCheck, GoalStatus, IntentCoverage, PlanSimulationError, SimulatedStep,
    SimulationObligation, SimulationReport, SimulationResult, compare_goal, simulate_plan,
    simulate_plan_for_goal, simulate_plan_for_goal_with_evidence,
    simulate_plan_for_goal_with_policy, simulate_plan_with_evidence,
};

pub use validate::{ProtectedAspect, ProtectedMount, ProtectedScope, ProtectionViolation};
