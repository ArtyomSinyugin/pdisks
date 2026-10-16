//! Editable user requests, separate from observed state and frozen execution plans.
//!
//! Deserialization does not validate semantics or authorize writes. Every session edit
//! rebuilds its preview through the existing composer and simulation checks.

mod build;
mod correlate;
mod coverage;
mod document;
mod draft;
mod lower;
mod preview;
mod rebase;
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

pub use session::{DEFAULT_HISTORY_LIMIT, EditError, EditReceipt, PlanEdit, PlanSession};

pub use document::{
    DRAFT_SCHEMA_VERSION, DraftDocument, DraftDocumentError, FreezeError, FrozenPlan,
    MAX_DRAFT_JSON_BYTES, MAX_DRAFT_OPERATIONS, PREVIEW_SCHEMA_VERSION, PreviewDocument,
};

pub use correlate::{CorrelationError, NodeCorrelation, correlate_nodes};

pub use rebase::{RebaseError, RebasePreview, remap_draft};
