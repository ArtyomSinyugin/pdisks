#![forbid(unsafe_code)]
#![cfg(target_os = "linux")]

//! Application facade: one configured host and dispatcher, with no alternate native write path.

mod error;
mod handles;
mod plans;
mod sessions;
pub use error::{DisksGraphError, ErrorCode};
pub use handles::{PlanId, RunId, SessionId};
pub use plans::MAX_PLAN_JSON_BYTES;
pub use storage_core::model;
pub use storage_engine::editing::{
    DraftDocument, DraftRevision, EditReceipt, FrozenPlan, OperationId, PlanEdit, PlanRequest,
    PreviewDocument, PreviewState, PreviewStatus, RebasePreview,
};
pub use storage_engine::{
    ActionId, ActionRunState, ActionStatus, BackendOutcome, EventsPage, LiveCheckStatus,
    LiveObservation, ObservationReference, PlanDocument, ProtectedScope, RunSnapshot,
};

/// Explicit trusted host configuration; imported plans cannot choose these providers or policies.
pub mod host {
    pub use storage_engine::{ExecutionContext, ExecutionRunError};
    pub use storage_probe::{
        LinuxProbe, ProbeSnapshot, ProviderProbeError, ProviderState, StateProvider,
    };
    pub use storage_provider::{
        BACKEND_MANIFEST_VERSION, BackendCapability, BackendId, BackendLibrary, BackendManifest,
        BackendManifests, BackendRegistry, EXT4_HOST_MOUNT_OPTIONS, LvmVgObservation,
        ProviderAction, ProviderConnectionKind, RegisteredBackend,
    };
    pub use storage_runtime::{
        authorization, capability, checkpoint, coordinator, dispatcher, host, incident, lease,
        native_ext4, native_lvm, native_mount, native_storage, probe, targets,
    };
}

use std::{collections::HashMap, path::Path};
use storage_engine::editing::PlanSession;
use storage_provider::{BackendId, ProviderAction};
use storage_runtime::{
    capability::OperationAvailability, checkpoint::CheckpointStore, dispatcher::BackendDispatcher,
    host::RuntimeHost, lease::ExecutionRoot,
};

/// Maximum retained objects per collection and maximum operation capability queries in one request.
pub const MAX_APPLICATION_OBJECTS: usize = 64;

/// Native recipe availability for one exact operation and the facade's configured dispatch route.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OperationCapability {
    /// Typed intent being queried, without credentials or executable strings.
    pub operation: ProviderAction,
    /// Fixed dispatch registration selected by the configured host.
    pub backend: BackendId,
    /// Native support alone does not attest coverage, approval or preservation.
    pub availability: OperationAvailability,
}
/// Advisory native support from one fresh pass; execution still performs all ordinary live checks.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OperationCapabilities {
    /// Exact actual publication used by the capability checker.
    pub observation: ObservationReference,
    /// An unresolved marker currently blocks writes even if native recipes are available.
    pub incident_pending: bool,
    /// Answers in the caller's operation order.
    pub operations: Vec<OperationCapability>,
}

/// Process-local application owner; trusted configuration is supplied separately from saved inputs.
pub struct DisksGraph<H, D> {
    /// Existing private directory shared by every cooperating application instance.
    root: ExecutionRoot,
    /// Actual observation, admission and final-verification owner.
    host: H,
    /// Sole configured typed native route, borrowed by the existing coordinator when executing.
    dispatcher: D,
    /// Opaque facade handles never reuse imported session identities.
    sessions: HashMap<SessionId, PlanSession>,
    /// Imported documents remain historical data, independently of editing sessions and live runs.
    plans: HashMap<PlanId, PlanDocument>,
}
impl<H: RuntimeHost, D: BackendDispatcher> DisksGraph<H, D> {
    /// Validates an existing private execution directory; never installs a permissive default policy.
    pub fn new(root: impl AsRef<Path>, host: H, dispatcher: D) -> Result<Self, DisksGraphError> {
        Ok(Self {
            root: ExecutionRoot::open(root)?,
            host,
            dispatcher,
            sessions: HashMap::new(),
            plans: HashMap::new(),
        })
    }
    /// Performs an actual provider pass without exposing a forecast as current state.
    pub fn probe(&mut self) -> Result<LiveObservation, DisksGraphError> {
        Ok(self.host.probe()?.into_observation())
    }
    /// Checks exact native routes and the current marker without granting permission to dispatch.
    pub fn capabilities(
        &mut self,
        operations: &[ProviderAction],
    ) -> Result<OperationCapabilities, DisksGraphError> {
        if operations.len() > MAX_APPLICATION_OBJECTS {
            return Err(DisksGraphError::new(
                ErrorCode::ResourceLimit,
                "too many capability queries",
            ));
        }
        let lock = self.root.try_lock_host()?;
        let incident_pending = CheckpointStore::new(&lock).inspect()?.is_some();
        let observed = self.host.probe()?;
        let operations = operations.iter().map(|operation| {
            let backend = self.dispatcher.backend_for(operation).clone();
            let mut availability = self.host.operation_availability(operation, &observed);
            if matches!(&availability, OperationAvailability::Available(selected) if selected != &backend) { availability = OperationAvailability::Unavailable("capability registration differs from the configured operation route".into()); }
            OperationCapability { operation: operation.clone(), backend, availability }
        }).collect();
        Ok(OperationCapabilities {
            observation: observed.observation().reference,
            incident_pending,
            operations,
        })
    }
}
