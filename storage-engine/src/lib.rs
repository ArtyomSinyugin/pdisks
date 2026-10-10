#![forbid(unsafe_code)]

//! Planning, validation, and simulation over trusted provider contracts.

mod projection;

pub use projection::{
    ActionSimulationError, CheckEvidence, CheckStatus, EffectApplicationError, PrerequisiteCheck,
    Projection, SimulatedAction,
};
