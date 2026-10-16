//! Typed synchronous dispatch boundary; no native write implementation is registered yet.

use crate::targets::ResolvedAction;
use storage_engine::{BackendOutcome, ExecutionRunError};
use storage_provider::BackendId;
use thiserror::Error;

/// A dispatch error does not prove absence of partial effects.
#[derive(Debug, Error)]
pub enum DispatchError {
    /// A native adapter could not classify completion safely.
    #[error("backend invocation failed: {0}")]
    Adapter(String),
    /// Progress could not be accepted by the owning execution run.
    #[error(transparent)]
    Progress(#[from] ExecutionRunError),
}

/// Trusted typed backend called only after admission and durable checkpoint publication.
/// Returns completed outcomes, not asynchronous launch acknowledgments; errors become unknown.
pub trait BackendDispatcher {
    /// Concrete adapter identity, checked against the admitted endpoint and checkpoint.
    fn backend_id(&self) -> &BackendId;
    /// Executes the immutable resolved action while the coordinator retains its OS lease.
    fn execute(
        &mut self,
        action: &ResolvedAction,
        progress: &mut dyn FnMut(String) -> Result<(), ExecutionRunError>,
    ) -> Result<BackendOutcome, DispatchError>;
}
