//! Recorded observations and an adapter that must never write, for application boundary tests.

// Shared with edition-2021 ABI tests; keep one import order across both rustfmt editions.
#[rustfmt::skip]
use disksgraph::{
    DisksGraph, LiveCheckStatus,
    host::{
        BackendId, LinuxProbe, ProbeSnapshot, ProviderConnectionKind, ProviderProbeError,
        ProviderState, StateProvider,
        authorization::{ApprovalRequest, AuthorizationContext},
        capability::{BackendCapabilityCheck, OperationAvailability},
        dispatcher::{BackendDispatcher, DispatchError, ExecutionReceipt},
        host::LinuxLiveHost,
        targets::ResolvedAction,
    },
};
use std::{
    cell::Cell,
    error::Error,
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    rc::Rc,
};

/// Private execution directory removed after the application releases its descriptor.
pub struct Scratch(pub PathBuf);
impl Scratch {
    /// Creates only a new fixture-owned directory with owner-only access.
    pub fn new() -> Result<Self, std::io::Error> {
        let path =
            std::env::temp_dir().join(format!("disksgraph-application-{}", uuid::Uuid::now_v7()));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    /// Removes only files created by this fixture, after all synchronous operations ended.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// An actual provider call over recorded data; no simulation state is injected.
struct Recorded {
    /// Immutable recorded graph and same-pass capacity records.
    snapshot: ProbeSnapshot,
    /// Counts actual adapter entries independently of snapshot generation.
    calls: Rc<Cell<usize>>,
}
impl StateProvider for Recorded {
    /// Distinguishes this fixture from native system providers.
    fn id(&self) -> &str {
        "application-recorded"
    }
    /// Owns this fixture's full private graph.
    fn responsibility(&self) -> &str {
        "application.recorded"
    }
    /// The adapter is an in-process provider.
    fn connection_kind(&self) -> ProviderConnectionKind {
        ProviderConnectionKind::BuiltIn
    }
    /// Counts each new publication and returns same-pass allocation data with the graph.
    fn probe(&self) -> Result<ProviderState, ProviderProbeError> {
        self.calls.set(self.calls.get() + 1);
        Ok(ProviderState {
            graph: self.snapshot.current.graph.clone(),
            mounts: self.snapshot.current.mounts.entries.clone(),
            lvm_vgs: self.snapshot.lvm_vgs.clone(),
            environment: Some(self.snapshot.current.environment.clone()),
            diagnostics: vec![],
        })
    }
}
/// No actual storage operation is implemented by this private fixture.
pub struct FixturePolicy;
impl BackendCapabilityCheck for FixturePolicy {
    /// Native support is unknown, so every nonempty plan must stop before dispatch.
    fn availability(
        &self,
        _: &storage_provider::ProviderAction,
        _: &disksgraph::LiveObservation,
    ) -> OperationAvailability {
        OperationAvailability::Unknown("fixture implements no writes".into())
    }
    /// An empty plan may independently verify the complete unchanged recorded graph only.
    fn check_final_scope(
        &self,
        _: &disksgraph::host::ExecutionContext<'_>,
        _: &disksgraph::LiveObservation,
    ) -> LiveCheckStatus {
        LiveCheckStatus::Satisfied
    }
}
/// The fixture cannot grant mutation authorization, including from imported documents.
pub struct NoApproval;
impl AuthorizationContext for NoApproval {
    /// Saved data never supplies a principal or destructive confirmation.
    fn check(&mut self, _: &ApprovalRequest) -> LiveCheckStatus {
        LiveCheckStatus::Unknown {
            required_evidence: "no fixture authorization".into(),
        }
    }
}
/// Any entry is a test failure signal; this adapter does not invoke a library or command.
pub struct NoWrites {
    /// Explicit synthetic backend route.
    backend: BackendId,
    /// Counts unexpected dispatcher entries.
    pub calls: Rc<Cell<usize>>,
}
impl BackendDispatcher for NoWrites {
    /// Keeps one explicit fixture route.
    fn backend_id(&self) -> &BackendId {
        &self.backend
    }
    /// Even a programming mistake cannot turn this fixture into a native write.
    fn execute(
        &mut self,
        _: &ResolvedAction,
        _: &mut dyn FnMut(String) -> Result<(), disksgraph::host::ExecutionRunError>,
    ) -> Result<ExecutionReceipt, DispatchError> {
        self.calls.set(self.calls.get() + 1);
        Err(DispatchError::Adapter("unexpected fixture dispatch".into()))
    }
}
/// Fully configured application with no runtime execution permission.
pub type Application = DisksGraph<LinuxLiveHost<'static, FixturePolicy, NoApproval>, NoWrites>;

/// Owned directory, configured facade, and independent probe/dispatch counters.
pub type Fixture = (Scratch, Application, Rc<Cell<usize>>, Rc<Cell<usize>>);

/// Creates independent host state and an existing private execution root.
pub fn application() -> Result<Fixture, Box<dyn Error>> {
    let scratch = Scratch::new()?;
    let (app, probes, writes) = open(&scratch.0)?;
    Ok((scratch, app, probes, writes))
}

/// A reopened application and its independent provider/dispatcher counters.
pub type OpenFixture = (Application, Rc<Cell<usize>>, Rc<Cell<usize>>);

/// Reopens an existing fixture root without manufacturing a new execution/lock domain.
pub fn open(root: &Path) -> Result<OpenFixture, Box<dyn Error>> {
    let probes = Rc::new(Cell::new(0));
    let writes = Rc::new(Cell::new(0));
    let snapshot = serde_json::from_str(include_str!(
        "../../../storage-engine/examples/fixtures/probe_snapshot.json"
    ))?;
    let probe = LinuxProbe::from_providers(vec![Box::new(Recorded {
        snapshot,
        calls: probes.clone(),
    })]);
    let host = LinuxLiveHost::new(probe, FixturePolicy, NoApproval);
    let dispatcher = NoWrites {
        backend: BackendId::new("application-no-writes").ok_or("backend ID")?,
        calls: writes.clone(),
    };
    let app = DisksGraph::new(root, host, dispatcher)?;
    Ok((app, probes, writes))
}
