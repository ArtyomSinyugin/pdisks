#![forbid(unsafe_code)]

//! Planning, validation, and simulation over trusted provider contracts.

mod compose;
mod plan;
mod projection;

pub use compose::{CompositionError, compose_plan};
pub use plan::{ActionDependency, ActionId, ExecutionPlan, FragmentId, PlannedAction};
pub use projection::{
    ActionSimulationError, CheckEvidence, CheckStatus, EffectApplicationError, PrerequisiteCheck,
    Projection, SimulatedAction,
};
