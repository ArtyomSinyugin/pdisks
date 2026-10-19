//! Operation-specific backend availability and trusted coverage evidence.

use crate::targets::ResolvedAction;
use storage_engine::{ExecutionContext, LiveCheckStatus, LiveObservation};
use storage_provider::{BackendId, ProviderAction};

/// Presence of a probe adapter alone never establishes native write support.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
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

    /// Independently excludes every graph, kernel and namespace consumer of the resolved backing LV.
    /// Must retain exclusion of mounts, external writes and DM table changes through dispatch and verification.
    /// Absence in the graph and general coverage do not establish endpoint exclusivity.
    fn check_consumer_exclusion(
        &self,
        _context: &ExecutionContext<'_>,
        _action: &ResolvedAction,
        _observed: &LiveObservation,
    ) -> LiveCheckStatus {
        LiveCheckStatus::Unknown {
            required_evidence: "independent backing endpoint and namespace consumer exclusion"
                .into(),
        }
    }

    /// Excludes outside source writers and mount-target/tree/namespace mutations through verification.
    /// A directory being empty at admission alone cannot establish that exclusion.
    fn check_mount_exclusion(
        &self,
        _context: &ExecutionContext<'_>,
        _action: &ResolvedAction,
        _observed: &LiveObservation,
    ) -> LiveCheckStatus {
        LiveCheckStatus::Unknown {
            required_evidence: "independent Host mount source, target and namespace exclusion"
                .into(),
        }
    }

    /// Establishes complete final observation/namespace coverage and exclusion of external writers.
    /// Operation admission coverage cannot silently stand in for this final publication's evidence.
    fn check_final_scope(
        &self,
        _context: &ExecutionContext<'_>,
        _observed: &LiveObservation,
    ) -> LiveCheckStatus {
        LiveCheckStatus::Unknown {
            required_evidence: "complete fresh final scope and namespace coverage".into(),
        }
    }

    /// Attests actual preservation throughout execution, including protected contents, not graph equality.
    /// An empty protection policy is vacuous; every nonempty policy needs independent host evidence.
    fn check_preservation(
        &self,
        context: &ExecutionContext<'_>,
        _observed: &LiveObservation,
    ) -> LiveCheckStatus {
        let protected = context.protected_scope;
        if protected.nodes.is_empty()
            && protected.runtime_mounts.is_empty()
            && protected.persistent_mounts.is_empty()
        {
            LiveCheckStatus::Satisfied
        } else {
            LiveCheckStatus::Unknown { required_evidence: "independent preservation evidence for protected contents and mount configuration".into() }
        }
    }
}
