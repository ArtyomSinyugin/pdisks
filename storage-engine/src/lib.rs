#![forbid(unsafe_code)]

//! Planning, validation, and simulation over trusted provider contracts.

mod compose;
mod plan;
mod projection;

pub use compose::{
    CompositionError, compose_plan, compose_plan_for_goal, compose_plan_for_goal_with_evidence,
    compose_plan_with_evidence,
};
pub use plan::{ActionDependency, ActionId, ExecutionPlan, FragmentId, PlannedAction};
pub use projection::{
    ActionSimulationError, CapacityEvidence, CapacityEvidenceError, CheckEvidence, CheckStatus,
    EffectApplicationError, PrerequisiteCheck, Projection, SimulatedAction,
};
