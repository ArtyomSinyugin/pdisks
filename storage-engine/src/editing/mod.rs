//! Editable user requests, separate from observed state and frozen execution plans.
//!
//! These are draft data only. Deserialization does not validate semantics or authorize
//! writes; request lowering and session editing will use the existing engine checks.

mod draft;
mod request;

pub use draft::{DraftRevision, OperationId, PlanDraft, PlanOperation};
pub use request::PlanRequest;
