//! Operation-specific backend availability and trusted coverage evidence.

use crate::targets::ResolvedAction;
use storage_engine::{ExecutionContext, LiveCheckStatus, LiveObservation};
use storage_provider::{BackendId, ProviderAction};

/// Presence of a probe adapter alone never establishes native write support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationAvailability {
    /// Trusted adapter has checked that it implements this exact operation on this host.
    Available(BackendId),
    /// A known missing capability prevents execution.
    Unavailable(String),
    /// Evidence is incomplete; the host must not invoke an adapter.
    Unknown(String),
}

/// Trust boundary for native capability and complete scope/namespace coverage checks.
/// Native operation support does not establish full scope coverage or authorization.
pub trait BackendCapabilityCheck {
    /// Checks the exact typed operation, not merely a backend's coarse Probe capability.
    fn availability(
        &self,
        operation: &ProviderAction,
        observed: &LiveObservation,
    ) -> OperationAvailability;

    /// Establishes complete affected-device and mount-namespace coverage beyond graph comparison.
    /// The default cannot attest completeness, so new adapters fail closed until implemented.
    fn check_scope(
        &self,
        _context: &ExecutionContext<'_>,
        _action: &ResolvedAction,
        _observed: &LiveObservation,
    ) -> LiveCheckStatus {
        LiveCheckStatus::Unknown {
            required_evidence: "complete provider and mount-namespace scope coverage".into(),
        }
    }
}
