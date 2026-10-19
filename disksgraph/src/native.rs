//! Owned native read configuration shared by CLI and long-lived application transports.

use crate::{DisksGraph, DisksGraphError, LiveCheckStatus, LiveObservation, host::*};
use authorization::{ApprovalRequest, AuthorizationContext};
use capability::{BackendCapabilityCheck, OperationAvailability};
use dispatcher::{BackendDispatcher, DispatchError, ExecutionReceipt};
use host::LinuxLiveHost;
use std::path::Path;
use targets::ResolvedAction;

/// Explicit configuration errors remain separate from facade/lease errors.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Missing registrations or invalid library/manifest configuration.
    #[error("{0}")]
    Configuration(String),
    /// An existing private root could not be opened safely.
    #[error(transparent)]
    Application(#[from] DisksGraphError),
}

/// No production scope or principal-bound authorization is inferred from configured reads.
pub struct ReadOnlyPolicy;
impl BackendCapabilityCheck for ReadOnlyPolicy {
    /// Native reads cannot attest that writes are implemented or safely authorized.
    fn availability(&self, _: &ProviderAction, _: &LiveObservation) -> OperationAvailability {
        OperationAvailability::Unknown("production write policies are not configured".into())
    }
}
impl AuthorizationContext for ReadOnlyPolicy {
    /// Saved inputs and invoker privileges do not supply exact action-bound approval.
    fn check(&mut self, _: &ApprovalRequest) -> LiveCheckStatus {
        LiveCheckStatus::Unknown {
            required_evidence: "principal and action-bound approval policy".into(),
        }
    }
}

/// Defense in depth: this default application has no mutation route.
pub struct DisabledDispatcher(BackendId);
impl BackendDispatcher for DisabledDispatcher {
    /// A fixed unavailable route, independent of imported backend names.
    fn backend_id(&self) -> &BackendId {
        &self.0
    }
    /// A mistaken positive admission cannot invoke any native mutator here.
    fn execute(
        &mut self,
        _: &ResolvedAction,
        _: &mut dyn FnMut(String) -> Result<(), ExecutionRunError>,
    ) -> Result<ExecutionReceipt, DispatchError> {
        Err(DispatchError::Adapter(
            "native writes are not configured".into(),
        ))
    }
}

/// Owns registrations while each probe temporarily borrows them; no leaks or self-references are needed.
struct OwnedSystemProvider(Option<BackendRegistry>);
impl StateProvider for OwnedSystemProvider {
    /// Keeps the native stack's existing provider identity.
    fn id(&self) -> &str {
        "native-linux-system-storage"
    }
    /// The complete stack owns one combined native contribution.
    fn responsibility(&self) -> &str {
        "storage.current-state"
    }
    /// Registrations are called in process through their existing native adapters.
    fn connection_kind(&self) -> ProviderConnectionKind {
        ProviderConnectionKind::BuiltIn
    }
    /// Reuses the established native assembly path while all registrations remain owned and live.
    fn probe(&self) -> Result<ProviderState, ProviderProbeError> {
        let registry = self.0.as_ref().ok_or_else(|| {
            ProviderProbeError::new(
                "native.not_configured",
                "this context supports saved inputs only",
            )
        })?;
        system(registry)
            .map_err(|error| ProviderProbeError::new("native.configuration", error.to_string()))?
            .probe()
    }
}

/// Resolves every required technology explicitly, without substituting an unrelated backend.
fn backend<'a>(
    registry: &'a BackendRegistry,
    id: &str,
) -> Result<&'a RegisteredBackend, OpenError> {
    registry
        .backends()
        .find(|backend| backend.manifest().id.as_str() == id)
        .ok_or_else(|| {
            OpenError::Configuration(format!("required native backend {id} is unavailable"))
        })
}

/// Constructs the same full stack as the native probe example, borrowed only for this provider call.
fn system(registry: &BackendRegistry) -> Result<NativeSystemProvider<'_>, OpenError> {
    let local = NativeLocalProvider::new(
        backend(registry, "udev-block")?,
        backend(registry, "fdisk-partitions")?,
        backend(registry, "blkid-signatures")?,
        backend(registry, "devmapper-topology")?,
        backend(registry, "cryptsetup-luks")?,
        backend(registry, "loop")?,
        backend(registry, "swap")?,
        backend(registry, "multipath")?,
        backend(registry, "lvm")?,
        backend(registry, "mdraid")?,
    );
    Ok(NativeSystemProvider::new(
        local,
        backend(registry, "libmount-mounts")?,
        backend(registry, "btrfs-subvolumes")?,
        backend(registry, "btrfs")?,
        backend(registry, "zfs")?,
        backend(registry, "nvme")?,
    ))
}

/// Complete owned application; native registrations drop with its provider, after all synchronous calls.
pub type Application =
    DisksGraph<LinuxLiveHost<'static, ReadOnlyPolicy, ReadOnlyPolicy>, DisabledDispatcher>;

/// Opens an existing private root with optional native reads and deliberately disabled writes.
/// Without a manifest, transports must restrict calls to saved inputs; direct probing reports missing data.
pub fn open(root: &Path, manifest: Option<&Path>) -> Result<Application, OpenError> {
    let registry = manifest
        .map(|path| {
            BackendRegistry::load_from(path)
                .map_err(|error| OpenError::Configuration(error.to_string()))
        })
        .transpose()?;
    if let Some(registry) = &registry {
        system(registry)?;
    }
    let probe = LinuxProbe::from_providers(vec![Box::new(OwnedSystemProvider(registry))]);
    let host = LinuxLiveHost::new(probe, ReadOnlyPolicy, ReadOnlyPolicy);
    let dispatcher =
        DisabledDispatcher(BackendId::new("cli-unconfigured").ok_or_else(|| {
            OpenError::Configuration("invalid unavailable backend identity".into())
        })?);
    Ok(DisksGraph::new(root, host, dispatcher)?)
}
