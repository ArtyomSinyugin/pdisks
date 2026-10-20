//! Owned native registrations with application-supplied policies and the ordinary coordinator.

use super::*;
use native_storage::NativeStorageAdapter;
use std::{cell::RefCell, rc::Rc};

/// Shared trusted host policy; each callback borrows it only for that one synchronous decision.
pub struct HostPolicy<P>(Rc<RefCell<P>>);
impl<P> HostPolicy<P> {
    /// Shares one application policy across admission, scope checks and incident acknowledgment.
    pub fn new(policy: P) -> Self {
        Self(Rc::new(RefCell::new(policy)))
    }
}
impl<P> Clone for HostPolicy<P> {
    /// Cloning shares host state rather than duplicating approval decisions or principal identity.
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<P: NativeHostPolicy> HostPolicy<P> {
    /// Forwards evidence to the embedding host without manufacturing a positive default.
    fn decide(&self, request: PolicyRequest<'_>) -> LiveCheckStatus {
        self.0.borrow_mut().check(request)
    }
}
impl<P: NativeHostPolicy> AuthorizationContext for HostPolicy<P> {
    /// Principal authority is supplied by the embedding application for this exact request.
    fn check(&mut self, request: &ApprovalRequest) -> LiveCheckStatus {
        self.decide(PolicyRequest::Authorize {
            request,
            destructive_target: None,
        })
    }
    /// Content replacement always requires its separate target-bound host decision.
    fn check_destructive(
        &mut self,
        request: &ApprovalRequest,
        target: crate::model::NodeId,
    ) -> LiveCheckStatus {
        self.decide(PolicyRequest::Authorize {
            request,
            destructive_target: Some(target),
        })
    }
}
impl<P: NativeHostPolicy> crate::IncidentAcknowledgmentPolicy for HostPolicy<P> {
    /// A challenge cannot replace the embedding host's independent incident safety policy.
    fn check(
        &mut self,
        reviewed: &crate::IncidentInspection,
        fresh: &crate::IncidentInspection,
    ) -> LiveCheckStatus {
        self.decide(PolicyRequest::Incident { reviewed, fresh })
    }
}

/// Owns native handles instead of borrowing a temporary adapter or leaking a registry.
#[derive(Clone)]
pub struct NativeDispatcher {
    /// Native providers and dispatcher share the same retained registrations.
    registry: Rc<BackendRegistry>,
    /// Preselected LVM route, independent of imported documents.
    lvm: BackendId,
    /// Preselected filesystem route.
    filesystems: BackendId,
    /// Preselected Host mount route.
    mounts: BackendId,
}
impl NativeDispatcher {
    /// Reborrows the same registrations for one ordinary typed availability or dispatch call.
    fn adapter(&self) -> Result<NativeStorageAdapter<'_>, OpenError> {
        NativeStorageAdapter::new(
            backend(&self.registry, self.lvm.as_str())?,
            backend(&self.registry, self.filesystems.as_str())?,
            backend(&self.registry, self.mounts.as_str())?,
        )
        .map_err(|error| OpenError::Configuration(error.to_string()))
    }
}
impl BackendDispatcher for NativeDispatcher {
    /// Primary route is retained for compatibility; each operation has its own exact registration.
    fn backend_id(&self) -> &BackendId {
        &self.lvm
    }
    /// Uses the same routing as NativeStorageAdapter, without a request-selected backend path.
    fn backend_for(&self, operation: &ProviderAction) -> &BackendId {
        match operation {
            ProviderAction::Filesystem(_) => &self.filesystems,
            ProviderAction::Mount(_) => &self.mounts,
            _ => &self.lvm,
        }
    }
    /// Native writes remain inside the existing endpoint guards and coordinator checkpoint sequence.
    fn execute(
        &mut self,
        action: &ResolvedAction,
        progress: &mut dyn FnMut(String) -> Result<(), ExecutionRunError>,
    ) -> Result<ExecutionReceipt, DispatchError> {
        self.adapter()
            .map_err(|error| DispatchError::Adapter(error.to_string()))?
            .execute(action, progress)
    }
}

/// Actual native availability plus host-owned scope/exclusion/final checks.
pub struct NativeCapabilities<P> {
    /// Same retained native route as dispatch.
    native: NativeDispatcher,
    /// Application owns every positive environmental attestation.
    policy: HostPolicy<P>,
}
impl<P: NativeHostPolicy> NativeCapabilities<P> {
    /// Supplies the full reviewed and fresh context for one precise kind of evidence.
    fn scope(
        &self,
        kind: ScopeCheck,
        context: &ExecutionContext<'_>,
        action: Option<&ResolvedAction>,
        observed: &LiveObservation,
    ) -> LiveCheckStatus {
        self.policy.decide(PolicyRequest::Scope {
            kind,
            context: context.into(),
            action,
            observed,
        })
    }
}
impl<P: NativeHostPolicy> BackendCapabilityCheck for NativeCapabilities<P> {
    /// A host policy cannot invent native support or override the configured backend registration.
    fn availability(
        &self,
        operation: &ProviderAction,
        observed: &LiveObservation,
    ) -> OperationAvailability {
        match self.native.adapter() {
            Ok(adapter) => adapter.availability(operation, observed),
            Err(error) => OperationAvailability::Unavailable(error.to_string()),
        }
    }
    /// Host attests complete action coverage separately from native support.
    fn check_scope(
        &self,
        context: &ExecutionContext<'_>,
        action: &ResolvedAction,
        observed: &LiveObservation,
    ) -> LiveCheckStatus {
        self.scope(ScopeCheck::Scope, context, Some(action), observed)
    }
    /// Host owns exclusion across the entire interval, beyond snapshot-based consumer checks.
    fn check_consumer_exclusion(
        &self,
        context: &ExecutionContext<'_>,
        action: &ResolvedAction,
        observed: &LiveObservation,
    ) -> LiveCheckStatus {
        self.scope(
            ScopeCheck::ConsumerExclusion,
            context,
            Some(action),
            observed,
        )
    }
    /// The same host must own the source, target tree and namespace through mount verification.
    fn check_mount_exclusion(
        &self,
        context: &ExecutionContext<'_>,
        action: &ResolvedAction,
        observed: &LiveObservation,
    ) -> LiveCheckStatus {
        self.scope(ScopeCheck::MountExclusion, context, Some(action), observed)
    }
    /// Final success still needs an independent fresh publication and environmental coverage.
    fn check_final_scope(
        &self,
        context: &ExecutionContext<'_>,
        observed: &LiveObservation,
    ) -> LiveCheckStatus {
        self.scope(ScopeCheck::FinalScope, context, None, observed)
    }
    /// The host must establish protected content preservation instead of relying on graph equality.
    fn check_preservation(
        &self,
        context: &ExecutionContext<'_>,
        observed: &LiveObservation,
    ) -> LiveCheckStatus {
        self.scope(ScopeCheck::Preservation, context, None, observed)
    }
}

/// Complete native application with externally supplied policy and owned registrations.
pub type WritableApplication<P> =
    DisksGraph<LinuxLiveHost<'static, NativeCapabilities<P>, HostPolicy<P>>, NativeDispatcher>;

/// Opens the supported native recipe only after explicit host policy and execute-enabled registrations.
/// Callers own authorization and exclusion; this does not imply support for every planning operation.
pub fn open_with_policy<P: NativeHostPolicy + 'static>(
    root: &Path,
    manifest: &Path,
    policy: HostPolicy<P>,
) -> Result<WritableApplication<P>, OpenError> {
    let (root, registry) = configuration(root, Some(manifest))?;
    let registry =
        registry.ok_or_else(|| OpenError::Configuration("native manifest is required".into()))?;
    let native = NativeDispatcher {
        lvm: backend(&registry, "lvm")?.manifest().id.clone(),
        filesystems: backend(&registry, "filesystems")?.manifest().id.clone(),
        mounts: backend(&registry, "libmount-mounts")?.manifest().id.clone(),
        registry: registry.clone(),
    };
    native.adapter()?;
    let probe = LinuxProbe::from_providers(vec![Box::new(OwnedSystemProvider(Some(registry)))]);
    let host = LinuxLiveHost::new(
        probe,
        NativeCapabilities {
            native: native.clone(),
            policy: policy.clone(),
        },
        policy,
    );
    Ok(DisksGraph::with_root(root, host, native))
}
