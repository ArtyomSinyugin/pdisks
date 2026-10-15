//! Editable user requests, separate from observed state and frozen execution plans.
//!
//! Deserialization does not validate semantics or authorize writes. Every session edit
//! rebuilds its preview through the existing composer and simulation checks.

mod build;
mod coverage;
mod draft;
mod lower;
mod preview;
mod request;
mod session;

pub use draft::{DraftRevision, OperationId, PlanDraft, PlanOperation};
pub use request::PlanRequest;

pub use lower::lower_operation;

pub use build::{
    DraftBuildError, OperationActions, OperationDiagnostic, PlanPreview, build_preview,
};

pub use coverage::RequestCoverage;

pub use preview::{PreviewState, PreviewStatus};

pub use session::{EditError, EditReceipt, PlanEdit, PlanSession};
