//! Explicit complete native read stack and deliberately unconfigured mutation policies.

use dg_cli::{Error, Invocation};
use disksgraph::{
    DisksGraph, LiveCheckStatus, LiveObservation,
    host::{
        BackendId, BackendRegistry, ExecutionRunError, LinuxProbe, NativeLocalProvider,
        NativeSystemProvider, ProviderAction, RegisteredBackend,
        authorization::{ApprovalRequest, AuthorizationContext},
        capability::{BackendCapabilityCheck, OperationAvailability},
        dispatcher::{BackendDispatcher, DispatchError, ExecutionReceipt},
        host::LinuxLiveHost,
        targets::ResolvedAction,
    },
};

/// No production scope or principal-bound authorization is fabricated by a CLI flag.
struct UnconfiguredPolicy;
impl BackendCapabilityCheck for UnconfiguredPolicy {
    /// Probe capability is never promoted to native execution readiness.
    fn availability(&self, _: &ProviderAction, _: &LiveObservation) -> OperationAvailability {
        OperationAvailability::Unknown("CLI production write policies are not configured".into())
    }
}
impl AuthorizationContext for UnconfiguredPolicy {
    /// Root and saved files cannot substitute for exact action-bound approval.
    fn check(&mut self, _: &ApprovalRequest) -> LiveCheckStatus {
        LiveCheckStatus::Unknown {
            required_evidence: "principal and action-bound approval policy".into(),
        }
    }
}
/// Defense in depth: this executable has no configured mutation route.
struct DisabledDispatcher(BackendId);
impl BackendDispatcher for DisabledDispatcher {
    /// Explicit unavailable route, independent of imported backend names.
    fn backend_id(&self) -> &BackendId {
        &self.0
    }
    /// Even an upstream admission regression cannot invoke a native mutator here.
    fn execute(
        &mut self,
        _: &ResolvedAction,
        _: &mut dyn FnMut(String) -> Result<(), ExecutionRunError>,
    ) -> Result<ExecutionReceipt, DispatchError> {
        Err(DispatchError::Adapter(
            "CLI native writes are not configured".into(),
        ))
    }
}

/// Resolves every technology explicitly, matching the established native probe example.
fn backend<'a>(registry: &'a BackendRegistry, id: &str) -> Result<&'a RegisteredBackend, Error> {
    registry
        .backends()
        .find(|backend| backend.manifest().id.as_str() == id)
        .ok_or_else(|| Error::Configuration(format!("required native backend {id} is unavailable")))
}

/// Keeps registrations alive for the entire facade invocation, including every fresh provider pass.
pub(super) fn run(invocation: &Invocation) -> Result<serde_json::Value, Error> {
    let registry = if invocation.command.needs_probe() {
        let manifest = invocation
            .manifest
            .as_ref()
            .ok_or_else(|| Error::Usage("required flag --manifest".into()))?;
        Some(
            BackendRegistry::load_from(manifest)
                .map_err(|error| Error::Configuration(error.to_string()))?,
        )
    } else {
        None
    };
    let providers: Vec<Box<dyn disksgraph::host::StateProvider>> = if let Some(registry) = &registry
    {
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
        let system = NativeSystemProvider::new(
            local,
            backend(registry, "libmount-mounts")?,
            backend(registry, "btrfs-subvolumes")?,
            backend(registry, "btrfs")?,
            backend(registry, "zfs")?,
            backend(registry, "nvme")?,
        );
        vec![Box::new(system)]
    } else {
        vec![]
    };
    let probe = LinuxProbe::from_providers(providers);
    let host = LinuxLiveHost::new(probe, UnconfiguredPolicy, UnconfiguredPolicy);
    let dispatcher = DisabledDispatcher(
        BackendId::new("cli-unconfigured")
            .ok_or_else(|| Error::Configuration("invalid CLI backend identity".into()))?,
    );
    let mut app = DisksGraph::new(&invocation.root, host, dispatcher)?;
    dg_cli::execute(&mut app, &invocation.command, &mut std::io::stderr().lock())
}
