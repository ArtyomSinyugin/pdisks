//! Editable user requests, separate from observed state and frozen execution plans.
//!
//! These are draft data only. Deserialization does not validate semantics or authorize
//! writes; request lowering and session editing will use the existing engine checks.

mod build;
mod draft;
mod lower;
mod request;

pub use draft::{DraftRevision, OperationId, PlanDraft, PlanOperation};
pub use request::PlanRequest;

pub use lower::lower_operation;

pub use build::{
    DraftBuildError, OperationActions, OperationDiagnostic, PlanPreview, build_preview,
};
