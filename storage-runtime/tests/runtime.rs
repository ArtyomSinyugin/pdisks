//! Synthetic runtime recipes exercise real locks and checkpoints, never native storage writes.

#[path = "../../storage-provider/tests/support/lvm.rs"]
mod mock_lvm;

#[path = "../../storage-provider/tests/support/ext4.rs"]
mod mock_ext4;

use std::{cell::RefCell, error::Error, rc::Rc};
use storage_core::model::{Bytes, ExternalId, NodeId, NodeKind};
use storage_engine::{
    ActionId, ExecutionRun, HistoricalBaseline,
    editing::{FrozenPlan, PlanSession},
};
use storage_probe::{LinuxProbe, ProbeSnapshot, ProviderProbeError, ProviderState, StateProvider};
use storage_provider::{BackendId, ProviderConnectionKind};
use storage_runtime::{
    probe::ProbedObservation,
    targets::{TargetError, resolve_action},
};

/// Forked C-fixture compilers temporarily inherit other threads' CLOEXEC lock descriptors.
/// Match the lease suite's process isolation so release assertions observe only their own guards.
static PROCESS_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes runtime test lifetimes with native-fixture compiler children in this test executable.
fn isolate_processes() -> std::sync::MutexGuard<'static, ()> {
    PROCESS_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// One mutable recorded provider, used to create fresh publication generations across dispatch.
struct Recorded(Rc<RefCell<ProbeSnapshot>>);
impl StateProvider for Recorded {
    /// Stable identity of this explicitly synthetic adapter.
    fn id(&self) -> &str {
        "runtime-fixture"
    }
    /// Owns this fixture's graph and same-pass allocation data.
    fn responsibility(&self) -> &str {
        "runtime.fixture"
    }
    /// The fixture is an in-process provider rather than a native library.
    fn connection_kind(&self) -> ProviderConnectionKind {
        ProviderConnectionKind::BuiltIn
    }
    /// Publishes exactly the current fixture snapshot without predictions.
    fn probe(&self) -> Result<ProviderState, ProviderProbeError> {
        let state = self.0.borrow();
        Ok(ProviderState {
            graph: state.current.graph.clone(),
            mounts: state.current.mounts.entries.clone(),
            lvm_vgs: state.lvm_vgs.clone(),
            environment: Some(state.current.environment.clone()),
            diagnostics: state.current.diagnostics.clone(),
        })
    }
}

/// Creates a frozen CreateLv plan from recorded VG measurements with a supported stable UUID.
fn fixture() -> Result<(ProbeSnapshot, FrozenPlan), Box<dyn Error>> {
    let mut state: ProbeSnapshot = serde_json::from_str(include_str!(
        "../../storage-engine/examples/fixtures/probe_snapshot.json"
    ))?;
    add_physical_backing(&mut state)?;
    let baseline =
        HistoricalBaseline::from_lvm_observations(state.current.clone(), &state.lvm_vgs)?;
    let mut session = PlanSession::new(baseline)?;
    session.add_lv(
        session.revision(),
        state.lvm_vgs[0].node,
        "data",
        Bytes::new(16 * 1024 * 1024),
    )?;
    Ok((state, session.freeze(session.revision())?))
}

/// Supplies the PV and physical disk that the recorded VG count says must exist.
fn add_physical_backing(state: &mut ProbeSnapshot) -> Result<(), Box<dyn Error>> {
    use storage_core::model::{
        BlockFacts, BlockGeometry, BlockSize, Dependency, DependencyKind, DeviceNumber, MemberRole,
    };
    let vg = state.lvm_vgs[0].node;
    let mut disk = state.current.graph.node(&vg).ok_or("VG")?.clone();
    disk.kind.kind = NodeKind::Disk;
    disk.size.identities = vec![ExternalId::Wwn("runtime-test-disk".into())];
    disk.size.block = Some(BlockFacts {
        paths: vec!["/dev/fixture-disk".into()],
        devno: DeviceNumber { major: 8, minor: 0 },
        read_only: Some(false),
        geometry: Some(BlockGeometry {
            logical_block_size: BlockSize::new(512).ok_or("block size")?,
            physical_block_size: BlockSize::new(4096),
            alignment_offset: Some(Bytes::new(0)),
            minimum_io_size: None,
            optimal_io_size: None,
        }),
    });
    let mut pv = disk.clone();
    pv.kind.kind = NodeKind::LvmPv;
    pv.size.identities = vec![ExternalId::LvmPvUuid("runtime-test-pv".into())];
    pv.size.block = None;
    let disk_id = NodeId::new();
    let pv_id = NodeId::new();
    state.current.graph.insert_new(disk_id, disk)?;
    state.current.graph.insert_new(pv_id, pv)?;
    state.current.graph.insert_dependency(Dependency {
        from: disk_id,
        to: pv_id,
        kind: DependencyKind::Backs,
    });
    state.current.graph.insert_dependency(Dependency {
        from: pv_id,
        to: vg,
        kind: DependencyKind::MemberOf(MemberRole::LvmPv),
    });
    Ok(())
}

/// Collects a real provider pass over explicitly synthetic state.
fn probe(state: Rc<RefCell<ProbeSnapshot>>) -> LinuxProbe<'static> {
    LinuxProbe::from_providers(vec![Box::new(Recorded(state))])
}

/// Test backend identity does not assert that an installed adapter can execute writes.
fn backend() -> BackendId {
    BackendId::new("fake-lvm").unwrap_or_else(|| unreachable!())
}

/// Transient IDs may change, but reused IDs/names with another VG UUID cannot select an endpoint.
#[test]
fn resolves_by_uuid_and_rejects_replacement_and_geometry_drift() -> Result<(), Box<dyn Error>> {
    let _process = isolate_processes();
    let (state, frozen) = fixture()?;
    let run = ExecutionRun::new(frozen.document())?;
    let old = state.lvm_vgs[0].node;
    let new = NodeId::new();
    let remapped: ProbeSnapshot = serde_json::from_str(
        &serde_json::to_string(&state)?
            .replace(&old.as_uuid().to_string(), &new.as_uuid().to_string()),
    )?;
    let publication = ProbedObservation::collect(&probe(Rc::new(RefCell::new(remapped))))?;
    let resolved = resolve_action(&run, ActionId::new(0), publication.observation(), backend())?;
    assert_eq!(resolved.target().planned_id, old);
    assert_eq!(resolved.target().observed_id, new);
    assert_eq!(resolved.target().vg_uuid, "example-vg-uuid");
    assert_eq!(resolved.observation(), publication.observation().reference);
    assert_eq!(resolved.run_id(), run.run_id());
    for case in 0..3 {
        let mut changed = state.clone();
        let mut node = changed.current.graph.node(&old).ok_or("VG")?.clone();
        match case {
            0 => node.size.identities = vec![ExternalId::LvmVgUuid("replacement".into())],
            1 => {
                if let NodeKind::LvmVg { extent_size, .. } = &mut node.kind.kind {
                    *extent_size = Some(Bytes::new(8 * 1024 * 1024));
                }
            }
            _ => node.size.identities.push(node.size.identities[0].clone()),
        }
        changed.current.graph.replace_existing(old, node)?;
        let observed = ProbedObservation::collect(&probe(Rc::new(RefCell::new(changed))))?;
        assert!(
            resolve_action(&run, ActionId::new(0), observed.observation(), backend()).is_err(),
            "case {case}"
        );
    }
    Ok(())
}

/// Extra scope cannot be hidden by preserving the VG UUID or a familiar display name.
#[test]
fn rejects_new_consumers_and_foreign_mounts() -> Result<(), Box<dyn Error>> {
    let _process = isolate_processes();
    use storage_core::model::{
        Dependency, DependencyKind, LvmLv, LvmLvKind, MountContext, MountSource, ObservedMount,
    };
    let (state, frozen) = fixture()?;
    let run = ExecutionRun::new(frozen.document())?;
    for case in 0..3 {
        let mut changed = state.clone();
        if case == 0 {
            changed.current.mounts.entries.push(ObservedMount {
                source: MountSource::Tmpfs,
                target: "/foreign".into(),
                options: vec![],
                context: MountContext::Host,
            });
        } else {
            let vg = changed.lvm_vgs[0].node;
            let id = NodeId::new();
            let mut lv = changed.current.graph.node(&vg).ok_or("VG")?.clone();
            lv.kind.kind = NodeKind::LvmLv(LvmLv {
                name: if case == 1 { "data" } else { "foreign" }.into(),
                kind: LvmLvKind::Linear,
            });
            lv.size.identities = vec![ExternalId::LvmLvUuid("foreign-lv".into())];
            changed.current.graph.insert_new(id, lv)?;
            changed.current.graph.insert_dependency(Dependency {
                from: vg,
                to: id,
                kind: DependencyKind::Provides,
            });
        }
        let observed = ProbedObservation::collect(&probe(Rc::new(RefCell::new(changed))))?;
        let error = resolve_action(&run, ActionId::new(0), observed.observation(), backend())
            .err()
            .ok_or("expected rejection")?;
        assert!(matches!(
            error,
            TargetError::ScopeChanged | TargetError::NameConflict
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod admission {
    use super::*;
    use std::{cell::Cell, fs, os::unix::fs::DirBuilderExt, path::PathBuf};
    use storage_engine::{ActionRunState, LiveCheckStatus};
    use storage_runtime::{
        authorization::{ApprovalRequest, AuthorizationContext},
        capability::{BackendCapabilityCheck, OperationAvailability},
        checkpoint::plan_digest,
        host::{HostError, LinuxLiveHost, RuntimeHost},
        lease::{ExecutionLease, ExecutionRoot},
    };

    /// Newly allocated private execution root, removed after every store and OS guard has dropped.
    pub(super) struct Scratch(pub(super) PathBuf);
    impl Scratch {
        /// Reserves a private test directory without touching existing user state.
        pub(super) fn new() -> Result<Self, Box<dyn Error>> {
            let path =
                std::env::temp_dir().join(format!("disksgraph-runtime-{}", uuid::Uuid::now_v7()));
            fs::DirBuilder::new().mode(0o700).create(&path)?;
            Ok(Self(path))
        }
    }
    impl Drop for Scratch {
        /// Production code retains roots and locks; only newly owned test fixtures are removed here.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Test-only capability provider; never used to claim installed native adapter support.
    pub(super) struct Capabilities {
        /// Explicit result of operation-specific synthetic availability checks.
        pub(super) availability: OperationAvailability,
        /// Explicit synthetic completeness evidence, independent of successful discovery.
        pub(super) scope: LiveCheckStatus,
    }
    impl BackendCapabilityCheck for Capabilities {
        /// Explicit final coverage for this synthetic private-memory fixture only.
        fn check_final_scope(
            &self,
            _context: &storage_engine::ExecutionContext<'_>,
            _observed: &storage_engine::LiveObservation,
        ) -> LiveCheckStatus {
            self.scope.clone()
        }
        /// Supplies the fixture's exact operation availability.
        fn availability(
            &self,
            _: &storage_provider::ProviderAction,
            _: &storage_engine::LiveObservation,
        ) -> OperationAvailability {
            self.availability.clone()
        }
        /// Positive coverage is an explicit test claim, never inferred from missing diagnostics.
        fn check_scope(
            &self,
            _: &storage_engine::ExecutionContext<'_>,
            _: &storage_runtime::targets::ResolvedAction,
            _: &storage_engine::LiveObservation,
        ) -> LiveCheckStatus {
            self.scope.clone()
        }
    }

    /// Test-only policy bound to the exact reviewed plan, backend, target, and current run.
    pub(super) struct Authorization {
        /// Digest the fixture explicitly reviewed.
        pub(super) digest: String,
        /// Attempt allowed by this fixture principal.
        pub(super) run_id: uuid::Uuid,
        /// Recorded policy decision; no production allow-all policy exists.
        pub(super) status: LiveCheckStatus,
        /// Confirms that policy is not consulted before other necessary evidence is complete.
        pub(super) calls: Rc<Cell<usize>>,
    }
    impl AuthorizationContext for Authorization {
        /// Verifies the complete request wiring before returning this fixture's principal decision.
        fn check(&mut self, request: &ApprovalRequest) -> LiveCheckStatus {
            self.calls.set(self.calls.get() + 1);
            if request.plan_digest != self.digest
                || request.run_id != self.run_id
                || request.backend != backend()
                || (request.targets.len() != 1
                    && !((2..=3).contains(&request.targets.len())
                        && (request.targets.len() == 2
                            || matches!(request.targets[2].external_ids.as_slice(), [ExternalId::Filesystem { fs_type, .. }] if fs_type == "ext4"))
                        && matches!(
                            request.targets[1].external_ids.as_slice(),
                            [ExternalId::LvmLvUuid(_)]
                        )))
                || request.observation.probe_instance.is_nil()
                || request.targets[0].external_ids
                    != vec![ExternalId::LvmVgUuid("example-vg-uuid".into())]
            {
                return LiveCheckStatus::Violated {
                    reason: "approval is bound to another plan, attempt or target".into(),
                };
            }
            self.status.clone()
        }
    }

    /// Constructs an explicit synthetic policy for one run, rather than serializing an approval flag.
    pub(super) fn authorization(run: &ExecutionRun) -> Result<Authorization, Box<dyn Error>> {
        Ok(Authorization {
            digest: plan_digest(run.document())?,
            run_id: run.run_id(),
            status: LiveCheckStatus::Satisfied,
            calls: Rc::new(Cell::new(0)),
        })
    }

    /// Every positive admission prerequisite is independent; unknown capability/scope never becomes true.
    #[test]
    fn admission_requires_capability_scope_and_exact_authorization() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        for case in 0..7 {
            let (state, frozen) = fixture()?;
            let scratch = Scratch::new()?;
            let root = ExecutionRoot::open(&scratch.0)?;
            let mut run = ExecutionRun::new(frozen.document())?;
            let action = ActionId::new(0);
            run.begin_check(action)?;
            let lease = ExecutionLease::try_acquire(&root, &run, action)?;
            let mut capability = Capabilities {
                availability: OperationAvailability::Available(backend()),
                scope: LiveCheckStatus::Satisfied,
            };
            let mut authorization = authorization(&run)?;
            let calls = authorization.calls.clone();
            let state = Rc::new(RefCell::new(state));
            match case {
                1 => {
                    capability.availability =
                        OperationAvailability::Unknown("no checked write adapter".into())
                }
                2 => {
                    capability.scope = LiveCheckStatus::Unknown {
                        required_evidence: "other mount namespaces".into(),
                    }
                }
                3 => {
                    authorization.status = LiveCheckStatus::Unknown {
                        required_evidence: "principal confirmation".into(),
                    }
                }
                4 => authorization.digest = "b".repeat(64),
                5 => state.borrow_mut().lvm_vgs[0].vg.free = 0,
                6 => {
                    capability.availability =
                        OperationAvailability::Unavailable("adapter cannot execute CreateLv".into())
                }
                _ => {}
            }
            let mut host = LinuxLiveHost::new(probe(state), capability, authorization);
            let observed = host.probe()?;
            let decision = host.check_admission(
                &run,
                action,
                &observed,
                &storage_runtime::bindings::OutputBindings::new(&run),
                &lease,
            )?;
            assert_eq!(decision.resolved.is_some(), case == 0);
            assert_eq!(run.record_admission(decision.report)?, case == 0);
            assert_eq!(
                run.state(action)?,
                match case {
                    0 => ActionRunState::Admitted,
                    4..=6 => ActionRunState::Failed,
                    _ => ActionRunState::Checking,
                }
            );
            assert_eq!(
                calls.get(),
                usize::from(case == 0 || case == 3 || case == 4)
            );
            assert!(!scratch.0.join("active-attempt.json").exists());
        }
        Ok(())
    }

    /// A report cannot reuse another host's publication, an old generation, or another run's guard.
    #[test]
    fn admission_rejects_foreign_lease_and_stale_publications() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        let (state, frozen) = fixture()?;
        let scratch = Scratch::new()?;
        let root = ExecutionRoot::open(&scratch.0)?;
        let mut run = ExecutionRun::new(frozen.document())?;
        let action = ActionId::new(0);
        run.begin_check(action)?;
        let lease = ExecutionLease::try_acquire(&root, &run, action)?;
        let mut other = ExecutionRun::new(frozen.document())?;
        other.begin_check(action)?;
        let state = Rc::new(RefCell::new(state));
        let mut host = LinuxLiveHost::new(
            probe(state.clone()),
            Capabilities {
                availability: OperationAvailability::Available(backend()),
                scope: LiveCheckStatus::Satisfied,
            },
            authorization(&run)?,
        );
        let observed = host.probe()?;
        assert!(matches!(
            host.check_admission(
                &other,
                action,
                &observed,
                &storage_runtime::bindings::OutputBindings::new(&other),
                &lease
            ),
            Err(HostError::LeaseMismatch)
        ));
        let foreign = ProbedObservation::collect(&probe(state))?;
        assert!(matches!(
            host.check_admission(
                &run,
                action,
                &foreign,
                &storage_runtime::bindings::OutputBindings::new(&run),
                &lease
            ),
            Err(HostError::ObservationMismatch)
        ));
        let latest = host.probe()?;
        assert!(matches!(
            host.check_admission(
                &run,
                action,
                &observed,
                &storage_runtime::bindings::OutputBindings::new(&run),
                &lease
            ),
            Err(HostError::ObservationMismatch)
        ));
        let decision = host.check_admission(
            &run,
            action,
            &latest,
            &storage_runtime::bindings::OutputBindings::new(&run),
            &lease,
        )?;
        assert_eq!(decision.report.run_id, run.run_id());
        assert_eq!(decision.report.held_resources, lease.held_resources());
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod coordination {
    use super::admission::{Authorization, Capabilities, Scratch, authorization};
    use super::*;
    use std::{cell::Cell, fs, path::PathBuf};
    use storage_engine::{
        ActionRunState, BackendOutcome, ExecutionRunError, LiveCheckStatus, RunEventKind,
    };
    use storage_runtime::{
        authorization::{ApprovalRequest, AuthorizationContext},
        capability::OperationAvailability,
        checkpoint::ActiveAttempt,
        coordinator::{CoordinatorError, ExecutionCoordinator},
        dispatcher::{BackendDispatcher, DispatchError, ExecutionReceipt},
        host::LinuxLiveHost,
        lease::ExecutionRoot,
        targets::ResolvedAction,
    };

    /// Explicitly synthetic backend that checks checkpoint ordering before changing fixture data.
    pub(super) struct FakeDispatcher {
        /// Exact adapter identity checked by the coordinator.
        pub(super) backend: BackendId,
        /// Parent-owned test root; used only to assert the active file was already published.
        pub(super) root: PathBuf,
        /// Current recorded publication, used to simulate a failed post-backend probe.
        pub(super) state: Rc<RefCell<ProbeSnapshot>>,
        /// Number of actual synthetic invocation attempts.
        pub(super) calls: Rc<Cell<usize>>,
        /// Completed classification supplied by the fixture, never by simulation.
        pub(super) outcome: BackendOutcome,
        /// Simulates the loss of a valid observation after invocation.
        pub(super) break_probe: bool,
        /// Simulates an adapter error whose partial effects cannot be ruled out.
        pub(super) fail: bool,
        /// UUID returned for a fixture-created LV, absent in generic success-only tests.
        pub(super) created_uuid: Option<String>,
        /// Selects deliberately incorrect result data for negative verification tests.
        pub(super) result_case: u8,
    }
    impl BackendDispatcher for FakeDispatcher {
        /// Identifies this test adapter without asserting native backend availability.
        fn backend_id(&self) -> &BackendId {
            &self.backend
        }
        /// Verifies durable-publication ordering, emits progress, and returns synthetic completion.
        fn execute(
            &mut self,
            action: &ResolvedAction,
            progress: &mut dyn FnMut(String) -> Result<(), ExecutionRunError>,
        ) -> Result<ExecutionReceipt, DispatchError> {
            let attempt = ActiveAttempt::from_bytes(
                &fs::read(self.root.join("active-attempt.json"))
                    .unwrap_or_else(|error| panic!("checkpoint before dispatch: {error}")),
            )
            .unwrap_or_else(|error| panic!("valid checkpoint: {error}"));
            assert_eq!(attempt.run_id, action.run_id());
            assert_eq!(attempt.action_id, action.action_id());
            assert_eq!(&attempt.action, action.operation());
            assert_eq!(&attempt.backend_id, action.backend());
            assert_eq!(attempt.targets, action.fingerprints());
            self.calls.set(self.calls.get() + 1);
            progress("synthetic adapter completed".into())?;
            if let storage_provider::ProviderAction::Lvm(
                storage_provider::LvmAction::SetActivation { active, .. },
            ) = action.operation()
            {
                let target = action
                    .lv_target()
                    .ok_or_else(|| DispatchError::Adapter("missing verified LV".into()))?;
                let mut state = self.state.borrow_mut();
                let mut node = state
                    .current
                    .graph
                    .node(&target.observed_id)
                    .cloned()
                    .ok_or_else(|| DispatchError::Adapter("missing fixture LV".into()))?;
                node.size.lvm_active = if self.result_case == 8 {
                    None
                } else {
                    Some(*active)
                };
                node.size.block = state.current.graph.nodes().find_map(|(_, node)| {
                    if matches!(node.kind.kind, NodeKind::Disk) {
                        node.size.block.clone()
                    } else {
                        None
                    }
                });
                if let Some(block) = &mut node.size.block {
                    block.paths = vec!["/dev/fixture-lv".into()];
                    block.read_only = Some(self.result_case == 9);
                }
                state
                    .current
                    .graph
                    .replace_existing(target.observed_id, node)
                    .map_err(|error| DispatchError::Adapter(error.to_string()))?;
                return Ok(ExecutionReceipt {
                    outcome: BackendOutcome::Succeeded,
                    created_lv_uuid: None,
                    created_filesystem_uuid: None,
                });
            }
            if self.break_probe {
                self.state.borrow_mut().current.graph.insert_dependency(
                    storage_core::model::Dependency {
                        from: NodeId::new(),
                        to: NodeId::new(),
                        kind: storage_core::model::DependencyKind::Backs,
                    },
                );
            }
            if let Some(uuid) = &self.created_uuid {
                use storage_core::model::{Dependency, DependencyKind, LvmLv};
                let storage_provider::ProviderAction::Lvm(storage_provider::LvmAction::CreateLv {
                    name,
                    size,
                    layout,
                    ..
                }) = action.operation()
                else {
                    panic!("fixture expects CreateLv");
                };
                // Only this fake adapter fabricates a test observation; production consumes native facts.
                let mut state = self.state.borrow_mut();
                let vg = action.target().observed_id;
                let mut lv = state
                    .current
                    .graph
                    .node(&vg)
                    .cloned()
                    .unwrap_or_else(|| panic!("fixture VG"));
                lv.kind.kind = NodeKind::LvmLv(LvmLv {
                    name: name.clone(),
                    kind: layout.clone(),
                });
                lv.kind.size = Some(if self.result_case == 1 {
                    Bytes::new(size.as_u64() / 2)
                } else {
                    *size
                });
                lv.size.identities = vec![ExternalId::LvmLvUuid(if self.result_case == 4 {
                    "unrelated-uuid".into()
                } else {
                    uuid.clone()
                })];
                let id = NodeId::new();
                state
                    .current
                    .graph
                    .insert_new(id, lv.clone())
                    .unwrap_or_else(|error| panic!("fixture LV: {error}"));
                let parent = if self.result_case == 2 {
                    let id = NodeId::new();
                    let mut other = state
                        .current
                        .graph
                        .node(&vg)
                        .cloned()
                        .unwrap_or_else(|| panic!("VG"));
                    other.size.identities = vec![ExternalId::LvmVgUuid("wrong-owner".into())];
                    state
                        .current
                        .graph
                        .insert_new(id, other)
                        .unwrap_or_else(|error| panic!("other VG: {error}"));
                    id
                } else {
                    vg
                };
                state.current.graph.insert_dependency(Dependency {
                    from: parent,
                    to: id,
                    kind: DependencyKind::Provides,
                });
                if self.result_case == 3 {
                    state
                        .current
                        .graph
                        .insert_new(NodeId::new(), lv)
                        .unwrap_or_else(|error| panic!("duplicate UUID: {error}"));
                }
                if self.result_case == 6 {
                    state
                        .current
                        .diagnostics
                        .push(storage_core::model::Diagnostic {
                            code: "fixture.incomplete".into(),
                            severity: storage_core::model::DiagnosticSeverity::Error,
                            subjects: vec![],
                            message: "incomplete result probe".into(),
                            evidence: None,
                            suggested_remedy: None,
                        });
                }
            }
            if self.result_case == 7 {
                fs::write(
                    self.root.join("active-attempt.json"),
                    b"unreadable incident",
                )
                .unwrap_or_else(|error| panic!("clear fault: {error}"));
            }
            if self.fail {
                Err(DispatchError::Adapter("completion transport lost".into()))
            } else {
                Ok(ExecutionReceipt {
                    outcome: self.outcome.clone(),
                    created_lv_uuid: self.created_uuid.clone(),
                    created_filesystem_uuid: None,
                })
            }
        }
    }

    /// Builds a fake backend with an inspectable invocation counter.
    pub(super) fn dispatcher(root: &Scratch, state: Rc<RefCell<ProbeSnapshot>>) -> FakeDispatcher {
        FakeDispatcher {
            backend: backend(),
            root: root.0.clone(),
            state,
            calls: Rc::new(Cell::new(0)),
            outcome: BackendOutcome::Succeeded,
            break_probe: false,
            fail: false,
            created_uuid: None,
            result_case: 0,
        }
    }

    /// Explicitly synthetic complete policies; production policies are never inferred from fixtures.
    pub(super) fn host(
        state: Rc<RefCell<ProbeSnapshot>>,
        run: &ExecutionRun,
    ) -> Result<LinuxLiveHost<'static, Capabilities, Authorization>, Box<dyn Error>> {
        Ok(LinuxLiveHost::new(
            probe(state),
            Capabilities {
                availability: OperationAvailability::Available(backend()),
                scope: LiveCheckStatus::Satisfied,
            },
            authorization(run)?,
        ))
    }

    /// Even a successful backend reply cannot verify an unobserved planned output or clear its marker.
    #[test]
    fn checkpoint_precedes_dispatch_and_missing_result_stops_continuation()
    -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        let (state, frozen) = fixture()?;
        let state = Rc::new(RefCell::new(state));
        let scratch = Scratch::new()?;
        let mut run = ExecutionRun::new(frozen.document())?;
        let fake = dispatcher(&scratch, state.clone());
        let calls = fake.calls.clone();
        let mut coordinator = ExecutionCoordinator::new(
            ExecutionRoot::open(&scratch.0)?,
            host(state, &run)?,
            fake,
            &run,
        );
        let operation = frozen.source_map()[0].operation;
        assert_eq!(
            coordinator.execute_action(&mut run, ActionId::new(0), Some(operation))?,
            ActionRunState::OutcomeUnknown
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(
            run.latest_observation()
                .ok_or("post probe")?
                .reference
                .epoch
                .get(),
            2
        );
        assert!(!run.is_verified());
        assert!(
            run.events_since(0)
                .any(|event| matches!(event.kind, RunEventKind::Progress { .. }))
        );
        let attempt = ActiveAttempt::from_bytes(&fs::read(scratch.0.join("active-attempt.json"))?)?;
        assert_eq!(attempt.operation_id, Some(operation));
        assert!(matches!(
            coordinator.execute_action(&mut run, ActionId::new(1), None),
            Err(CoordinatorError::Stopped)
        ));
        assert_eq!(calls.get(), 1);
        Ok(())
    }

    /// Every incomplete or contradicted live gate prevents the synthetic call and any checkpoint.
    #[test]
    fn every_admission_veto_prevents_dispatch() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        for case in 0..6 {
            let (state, frozen) = fixture()?;
            let state = Rc::new(RefCell::new(state));
            let scratch = Scratch::new()?;
            let mut run = ExecutionRun::new(frozen.document())?;
            let mut capabilities = Capabilities {
                availability: OperationAvailability::Available(backend()),
                scope: LiveCheckStatus::Satisfied,
            };
            let mut authorization = authorization(&run)?;
            match case {
                0 => {
                    capabilities.availability =
                        OperationAvailability::Unknown("write unavailable".into())
                }
                1 => {
                    capabilities.scope = LiveCheckStatus::Unknown {
                        required_evidence: "namespace coverage".into(),
                    }
                }
                2 => {
                    authorization.status = LiveCheckStatus::Violated {
                        reason: "no approval".into(),
                    }
                }
                3 => {
                    let id = state.borrow().lvm_vgs[0].node;
                    let mut node = state.borrow().current.graph.node(&id).ok_or("VG")?.clone();
                    node.size.identities = vec![ExternalId::LvmVgUuid("other-vg".into())];
                    state
                        .borrow_mut()
                        .current
                        .graph
                        .replace_existing(id, node)?;
                }
                4 => state.borrow_mut().current.mounts.entries.push(
                    storage_core::model::ObservedMount {
                        source: storage_core::model::MountSource::Tmpfs,
                        target: "/foreign".into(),
                        options: vec![],
                        context: storage_core::model::MountContext::Host,
                    },
                ),
                _ => state.borrow_mut().lvm_vgs[0].vg.free = 0,
            }
            let fake = dispatcher(&scratch, state.clone());
            let calls = fake.calls.clone();
            let mut coordinator = ExecutionCoordinator::new(
                ExecutionRoot::open(&scratch.0)?,
                LinuxLiveHost::new(probe(state), capabilities, authorization),
                fake,
                &run,
            );
            coordinator.execute_action(&mut run, ActionId::new(0), None)?;
            assert_eq!(calls.get(), 0, "case {case}");
            assert!(!scratch.0.join("active-attempt.json").exists());
        }
        Ok(())
    }

    /// Known failure, uncertain adapter error, and post-probe failure retain the marker and stop.
    #[test]
    fn backend_failure_and_probe_loss_retain_checkpoint() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        for case in 0..3 {
            let (state, frozen) = fixture()?;
            let state = Rc::new(RefCell::new(state));
            let scratch = Scratch::new()?;
            let mut run = ExecutionRun::new(frozen.document())?;
            let mut fake = dispatcher(&scratch, state.clone());
            match case {
                0 => {
                    fake.outcome = BackendOutcome::Failed {
                        reason: "partial failure".into(),
                    }
                }
                1 => fake.fail = true,
                _ => fake.break_probe = true,
            }
            let calls = fake.calls.clone();
            let mut coordinator = ExecutionCoordinator::new(
                ExecutionRoot::open(&scratch.0)?,
                host(state, &run)?,
                fake,
                &run,
            );
            let result = coordinator.execute_action(&mut run, ActionId::new(0), None);
            assert_eq!(result.is_err(), case == 2);
            assert_eq!(
                run.state(ActionId::new(0))?,
                if case == 0 {
                    ActionRunState::Failed
                } else {
                    ActionRunState::OutcomeUnknown
                }
            );
            assert_eq!(calls.get(), 1);
            assert!(scratch.0.join("active-attempt.json").exists());
            assert_eq!(
                run.latest_observation()
                    .ok_or("actual")?
                    .reference
                    .epoch
                    .get(),
                if case == 2 { 1 } else { 2 }
            );
            assert!(matches!(
                coordinator.execute_action(&mut run, ActionId::new(0), None),
                Err(CoordinatorError::Stopped)
            ));
        }
        Ok(())
    }

    /// A test policy creates a competing marker after inspection to force preparation failure.
    struct CheckpointCollision {
        /// Ordinary exact-request fixture authorization.
        inner: Authorization,
        /// Newly owned test directory where the competing marker is injected.
        root: PathBuf,
    }
    impl AuthorizationContext for CheckpointCollision {
        /// Injects only a temporary regular-file collision; no storage backend runs.
        fn check(&mut self, request: &ApprovalRequest) -> LiveCheckStatus {
            use std::os::unix::fs::PermissionsExt;
            let path = self.root.join("active-attempt.json");
            fs::write(&path, b"competing incident")
                .unwrap_or_else(|error| panic!("collision: {error}"));
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .unwrap_or_else(|error| panic!("collision permissions: {error}"));
            self.inner.check(request)
        }
    }

    /// An arm failure stays before invocation, retains the competing data, and records its reason.
    #[test]
    fn checkpoint_failure_stops_before_dispatch() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        let (state, frozen) = fixture()?;
        let state = Rc::new(RefCell::new(state));
        let scratch = Scratch::new()?;
        let mut run = ExecutionRun::new(frozen.document())?;
        let fake = dispatcher(&scratch, state.clone());
        let calls = fake.calls.clone();
        let authorization = CheckpointCollision {
            inner: authorization(&run)?,
            root: scratch.0.clone(),
        };
        let host = LinuxLiveHost::new(
            probe(state),
            Capabilities {
                availability: OperationAvailability::Available(backend()),
                scope: LiveCheckStatus::Satisfied,
            },
            authorization,
        );
        let mut coordinator =
            ExecutionCoordinator::new(ExecutionRoot::open(&scratch.0)?, host, fake, &run);
        assert!(matches!(
            coordinator.execute_action(&mut run, ActionId::new(0), None),
            Err(CoordinatorError::Checkpoint(_))
        ));
        assert_eq!(run.state(ActionId::new(0))?, ActionRunState::Failed);
        assert!(run.pre_dispatch_failure(ActionId::new(0))?.is_some());
        assert!(run.completion(ActionId::new(0))?.is_none());
        assert_eq!(calls.get(), 0);
        assert_eq!(
            fs::read(scratch.0.join("active-attempt.json"))?,
            b"competing incident"
        );
        Ok(())
    }
    /// Permission changes and lock replacement at callback boundaries cannot arm or clear an attempt.
    #[test]
    fn callback_lock_changes_stop_before_arm_or_successor() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;
        let _process = isolate_processes();
        for before_clear in [false, true] {
            for replace_lock in [false, true] {
                let (state, frozen) = fixture()?;
                let state = Rc::new(RefCell::new(state));
                let scratch = Scratch::new()?;
                let mut run = ExecutionRun::new(frozen.document())?;
                let mut fake = dispatcher(&scratch, state.clone());
                fake.created_uuid = Some("callback-lv-uuid".into());
                let calls = fake.calls.clone();
                let mut coordinator = ExecutionCoordinator::new(
                    ExecutionRoot::open(&scratch.0)?,
                    host(state, &run)?,
                    fake,
                    &run,
                );
                let mut changed = false;
                let mut bytes = Vec::new();
                let result = coordinator.execute_run_with_updates(&mut run, &mut |run| {
                    let phase = if before_clear {
                        ActionRunState::Verified
                    } else {
                        ActionRunState::Admitted
                    };
                    if changed || run.state(ActionId::new(0)).ok() != Some(phase) {
                        return;
                    }
                    changed = true;
                    if before_clear {
                        bytes = fs::read(scratch.0.join("active-attempt.json"))
                            .unwrap_or_else(|error| panic!("read armed marker: {error}"));
                    }
                    if replace_lock {
                        fs::rename(
                            scratch.0.join("execution.lock"),
                            scratch.0.join("retained.lock"),
                        )
                        .unwrap_or_else(|error| panic!("replace callback lock: {error}"));
                    } else {
                        fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o755))
                            .unwrap_or_else(|error| panic!("change callback permissions: {error}"));
                    }
                });
                assert!(changed);
                assert!(matches!(result, Err(CoordinatorError::Checkpoint(_))));
                assert!(!run.is_verified());
                assert_eq!(calls.get(), usize::from(before_clear));
                assert_eq!(run.state(ActionId::new(1))?, ActionRunState::Waiting);
                if before_clear {
                    assert_eq!(fs::read(scratch.0.join("active-attempt.json"))?, bytes);
                    assert!(matches!(
                        coordinator.execute_run(&mut run),
                        Err(CoordinatorError::Stopped)
                    ));
                } else {
                    assert_eq!(run.state(ActionId::new(0))?, ActionRunState::Failed);
                    assert!(!scratch.0.join("active-attempt.json").exists());
                }
                fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }

    /// Receipt UUID plus independent LV properties/ownership is required before clear or binding.
    #[test]
    fn created_lv_binding_requires_verified_result_and_exact_owner() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        use storage_provider::{LvmAction, ProviderAction};
        for case in 0..7 {
            let (state, frozen) = fixture()?;
            let state = Rc::new(RefCell::new(state));
            if case == 5 {
                let vg = state.borrow().lvm_vgs[0].node;
                let mut node = state.borrow().current.graph.node(&vg).ok_or("VG")?.clone();
                node.kind.kind = NodeKind::LvmLv(storage_core::model::LvmLv {
                    name: "preexisting".into(),
                    kind: storage_core::model::LvmLvKind::Linear,
                });
                node.size.identities = vec![ExternalId::LvmLvUuid("created-lv-uuid".into())];
                state
                    .borrow_mut()
                    .current
                    .graph
                    .insert_new(NodeId::new(), node)?;
            }
            let scratch = Scratch::new()?;
            let mut run = ExecutionRun::new(frozen.document())?;
            let ProviderAction::Lvm(LvmAction::CreateLv {
                planned_node_id, ..
            }) = &run.context().plan.actions()[0].operation
            else {
                return Err("CreateLv".into());
            };
            let planned = *planned_node_id;
            let mut fake = dispatcher(&scratch, state.clone());
            fake.created_uuid = Some("created-lv-uuid".into());
            fake.result_case = case;
            let calls = fake.calls.clone();
            let mut coordinator = ExecutionCoordinator::new(
                ExecutionRoot::open(&scratch.0)?,
                host(state, &run)?,
                fake,
                &run,
            );
            assert!(coordinator.bindings().is_empty());
            let result = coordinator.execute_action(&mut run, ActionId::new(0), None);
            assert_eq!(result.is_err(), (2..=5).contains(&case), "case {case}");
            assert_eq!(calls.get(), 1);
            assert_eq!(
                run.state(ActionId::new(0))?,
                match case {
                    0 => ActionRunState::Verified,
                    1 => ActionRunState::Failed,
                    _ => ActionRunState::OutcomeUnknown,
                }
            );
            assert_eq!(coordinator.bindings().len(), usize::from(case == 0));
            assert_eq!(scratch.0.join("active-attempt.json").exists(), case != 0);
            assert!(!run.is_verified());
            if case == 0 {
                assert_eq!(fs::read_dir(&scratch.0)?.count(), 1);
                let actual = run.latest_observation().ok_or("actual result")?;
                let id = coordinator.bindings().resolve(&run, planned, actual)?;
                assert_ne!(id, planned);
                assert!(
                    actual
                        .snapshot
                        .graph
                        .node(&id)
                        .ok_or("LV")?
                        .size
                        .identities
                        .contains(&ExternalId::LvmLvUuid("created-lv-uuid".into()))
                );
                let mut replacement = actual.clone();
                let mut node = replacement.snapshot.graph.node(&id).ok_or("LV")?.clone();
                node.size.identities = vec![ExternalId::LvmLvUuid("replacement".into())];
                replacement.snapshot.graph.replace_existing(id, node)?;
                assert!(
                    coordinator
                        .bindings()
                        .resolve(&run, planned, &replacement)
                        .is_err()
                );
                let mut other = ExecutionRun::new(frozen.document())?;
                assert!(
                    coordinator
                        .bindings()
                        .resolve(&other, planned, actual)
                        .is_err()
                );
                assert!(matches!(
                    coordinator.execute_action(&mut other, ActionId::new(0), None),
                    Err(CoordinatorError::RunMismatch)
                ));
                assert_eq!(other.state(ActionId::new(0))?, ActionRunState::Waiting);
            }
        }
        Ok(())
    }
    /// Neither a different adapter nor an old unresolved incident may reach the dispatcher.
    #[test]
    fn backend_mismatch_and_existing_incident_prevent_dispatch() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        for existing in [false, true] {
            let (state, frozen) = fixture()?;
            let state = Rc::new(RefCell::new(state));
            let scratch = Scratch::new()?;
            let mut run = ExecutionRun::new(frozen.document())?;
            let mut fake = dispatcher(&scratch, state.clone());
            if existing {
                fs::write(
                    scratch.0.join("active-attempt.json"),
                    b"unresolved incident",
                )?;
            } else {
                fake.backend = BackendId::new("different-adapter").ok_or("backend")?;
            }
            let calls = fake.calls.clone();
            let mut coordinator = ExecutionCoordinator::new(
                ExecutionRoot::open(&scratch.0)?,
                host(state, &run)?,
                fake,
                &run,
            );
            assert!(
                coordinator
                    .execute_action(&mut run, ActionId::new(0), None)
                    .is_err()
            );
            assert_eq!(calls.get(), 0);
            assert_eq!(run.state(ActionId::new(0))?, ActionRunState::Checking);
            assert!(coordinator.bindings().is_empty());
            if existing {
                assert_eq!(
                    fs::read(scratch.0.join("active-attempt.json"))?,
                    b"unresolved incident"
                );
            } else {
                assert!(!scratch.0.join("active-attempt.json").exists());
            }
        }
        Ok(())
    }
    /// Verified output cannot authorize a successor when checkpoint clear fails afterwards.
    #[test]
    fn clear_failure_keeps_coordinator_stopped_after_verified_result() -> Result<(), Box<dyn Error>>
    {
        let _process = isolate_processes();
        let (state, frozen) = fixture()?;
        let state = Rc::new(RefCell::new(state));
        let scratch = Scratch::new()?;
        let mut run = ExecutionRun::new(frozen.document())?;
        let mut fake = dispatcher(&scratch, state.clone());
        fake.created_uuid = Some("created-lv-uuid".into());
        fake.result_case = 7;
        let calls = fake.calls.clone();
        let mut coordinator = ExecutionCoordinator::new(
            ExecutionRoot::open(&scratch.0)?,
            host(state, &run)?,
            fake,
            &run,
        );
        assert!(matches!(
            coordinator.execute_action(&mut run, ActionId::new(0), None),
            Err(CoordinatorError::Checkpoint(_))
        ));
        assert_eq!(run.state(ActionId::new(0))?, ActionRunState::Verified);
        assert_eq!(coordinator.bindings().len(), 1);
        assert!(matches!(
            coordinator.execute_action(&mut run, ActionId::new(1), None),
            Err(CoordinatorError::Stopped)
        ));
        assert_eq!(calls.get(), 1);
        assert_eq!(
            fs::read(scratch.0.join("active-attempt.json"))?,
            b"unreadable incident"
        );
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod incidents {
    use super::*;
    use super::{
        admission::Scratch,
        coordination::{dispatcher, host},
    };
    use storage_runtime::{
        checkpoint::{ActiveAttempt, CheckpointStore},
        coordinator::ExecutionCoordinator,
        incident::{IncidentAssessment, inspect_incident},
        lease::ExecutionRoot,
    };

    /// Leaves the same marker a stopped coordinator produces, using only a synthetic adapter.
    fn interrupted(
        root: &Scratch,
        state: Rc<RefCell<ProbeSnapshot>>,
        frozen: &FrozenPlan,
    ) -> Result<(ActiveAttempt, ExecutionRun), Box<dyn Error>> {
        let mut run = ExecutionRun::new(frozen.document())?;
        let fake = dispatcher(root, state.clone());
        let mut coordinator = ExecutionCoordinator::new(
            ExecutionRoot::open(&root.0)?,
            host(state, &run)?,
            fake,
            &run,
        );
        coordinator.execute_action(&mut run, ActionId::new(0), None)?;
        let lock = ExecutionRoot::open(&root.0)?.try_lock_host()?;
        Ok((
            CheckpointStore::new(&lock)
                .inspect()?
                .ok_or("active attempt")?,
            run,
        ))
    }

    /// Compatible observed properties do not remove the marker or recover any execution state.
    #[test]
    fn inspection_is_fresh_read_only_and_conservative() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        let (state, frozen) = fixture()?;
        let state = Rc::new(RefCell::new(state));
        let scratch = Scratch::new()?;
        let (attempt, run) = interrupted(&scratch, state.clone(), &frozen)?;
        let root = ExecutionRoot::open(&scratch.0)?;
        let probe = probe(state.clone());
        let bytes = std::fs::read(scratch.0.join("active-attempt.json"))?;
        let first = inspect_incident(&root, &probe)?.ok_or("inspection")?;
        assert_eq!(first.attempt, attempt);
        assert_eq!(first.assessment, IncidentAssessment::Indeterminate);
        let mut fake = dispatcher(&scratch, state.clone());
        // Materialize only a recorded result fixture; this adapter never mutates host storage.
        fake.created_uuid = Some("inspected-lv".into());
        let observed = ProbedObservation::collect(&probe)?;
        let resolved = resolve_action(&run, ActionId::new(0), observed.observation(), backend())?;
        use storage_runtime::dispatcher::BackendDispatcher;
        fake.execute(&resolved, &mut |_| Ok(()))?;
        let second = inspect_incident(&root, &probe)?.ok_or("inspection")?;
        assert_eq!(second.assessment, IncidentAssessment::EffectObserved);
        assert!(second.fresh_state.reference.epoch > first.fresh_state.reference.epoch);
        assert_eq!(std::fs::read(scratch.0.join("active-attempt.json"))?, bytes);
        assert_eq!(std::fs::read_dir(&scratch.0)?.count(), 2);
        let id = state
            .borrow()
            .current
            .graph
            .nodes()
            .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::LvmLv(_)).then_some(*id))
            .ok_or("LV")?;
        let mut duplicate = state.borrow().current.graph.node(&id).ok_or("LV")?.clone();
        duplicate.kind.size = Some(Bytes::new(1));
        state
            .borrow_mut()
            .current
            .graph
            .replace_existing(id, duplicate)?;
        assert_eq!(
            inspect_incident(&root, &probe)?
                .ok_or("inspection")?
                .assessment,
            IncidentAssessment::Indeterminate
        );
        Ok(())
    }

    /// No incident needs no provider pass; unsafe or corrupt state remains a blocking error.
    #[test]
    fn inspection_rejects_busy_lock_corruption_and_probe_failure() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        let (state, frozen) = fixture()?;
        let state = Rc::new(RefCell::new(state));
        let scratch = Scratch::new()?;
        let root = ExecutionRoot::open(&scratch.0)?;
        let probe = probe(state.clone());
        assert!(inspect_incident(&root, &probe)?.is_none());
        let lock = root.try_lock_host()?;
        assert!(inspect_incident(&root, &probe).is_err());
        drop(lock);
        let (attempt, _) = interrupted(&scratch, state.clone(), &frozen)?;
        state
            .borrow_mut()
            .current
            .graph
            .insert_dependency(storage_core::model::Dependency {
                from: NodeId::new(),
                to: NodeId::new(),
                kind: storage_core::model::DependencyKind::Backs,
            });
        assert!(inspect_incident(&root, &probe).is_err());
        let bytes = attempt.to_bytes()?;
        assert_eq!(std::fs::read(scratch.0.join("active-attempt.json"))?, bytes);
        std::fs::write(scratch.0.join("active-attempt.json"), b"corrupt")?;
        assert!(inspect_incident(&root, &probe).is_err());
        assert_eq!(
            std::fs::read(scratch.0.join("active-attempt.json"))?,
            b"corrupt"
        );
        Ok(())
    }
    /// Explicit fixture policy checks fresh probe ordering and real lock lifetime during acknowledgment.
    struct AckPolicy {
        /// Root for verifying that the ordinary execution lock remains held.
        root: std::path::PathBuf,
        /// Synthetic principal/quiescence decision; no native operation is running in these tests.
        status: storage_engine::LiveCheckStatus,
        /// Optional fault injection replacing the marker after the safety check.
        replace: bool,
        /// Number of safety checks performed after confirmation and unchanged-state checks.
        calls: usize,
    }
    impl storage_runtime::incident::IncidentAcknowledgmentPolicy for AckPolicy {
        /// Never derives quiescence from the graph; the fixture explicitly supplies that evidence.
        fn check(
            &mut self,
            reviewed: &storage_runtime::incident::IncidentInspection,
            fresh: &storage_runtime::incident::IncidentInspection,
        ) -> storage_engine::LiveCheckStatus {
            self.calls += 1;
            assert!(fresh.fresh_state.reference.epoch > reviewed.fresh_state.reference.epoch);
            assert!(matches!(
                ExecutionRoot::open(&self.root)
                    .unwrap_or_else(|e| panic!("root: {e}"))
                    .try_lock_host(),
                Err(storage_runtime::lease::LeaseError::Busy)
            ));
            if self.replace {
                let mut replacement = fresh.attempt.clone();
                replacement.backend_id =
                    BackendId::new("replacement-backend").unwrap_or_else(|| unreachable!());
                std::fs::write(
                    self.root.join("active-attempt.json"),
                    replacement
                        .to_bytes()
                        .unwrap_or_else(|e| panic!("encode: {e}")),
                )
                .unwrap_or_else(|e| panic!("replace: {e}"));
            }
            self.status.clone()
        }
    }

    /// Confirmation never marks the old run verified or resumes its action; unsafe evidence retains the marker.
    #[test]
    fn acknowledgment_requires_exact_confirmation_and_quiescence() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        use storage_engine::{ActionRunState, LiveCheckStatus};
        use storage_runtime::incident::{acknowledge_incident, review_incident};
        for case in 0..5 {
            let (state, frozen) = fixture()?;
            let state = Rc::new(RefCell::new(state));
            let scratch = Scratch::new()?;
            let (_, run) = interrupted(&scratch, state.clone(), &frozen)?;
            let root = ExecutionRoot::open(&scratch.0)?;
            let probe = probe(state);
            let review = review_incident(&root, &probe)?.ok_or("review")?;
            let token = if case == 1 {
                uuid::Uuid::now_v7()
            } else {
                review.confirmation_token()
            };
            let mut policy = AckPolicy {
                root: scratch.0.clone(),
                status: match case {
                    2 => LiveCheckStatus::Unknown {
                        required_evidence: "backend may still be running".into(),
                    },
                    3 => LiveCheckStatus::Violated {
                        reason: "principal has no incident authorization".into(),
                    },
                    _ => LiveCheckStatus::Satisfied,
                },
                replace: case == 4,
                calls: 0,
            };
            let result = acknowledge_incident(&root, &probe, review, token, &mut policy);
            assert_eq!(result.is_ok(), case == 0, "case {case}: {result:?}");
            assert_eq!(policy.calls, usize::from(case != 1));
            assert_eq!(scratch.0.join("active-attempt.json").exists(), case != 0);
            assert_eq!(run.state(ActionId::new(0))?, ActionRunState::OutcomeUnknown);
            assert!(!run.is_verified());
        }
        Ok(())
    }

    /// Another root, marker replacement, or new observed state invalidates review before policy or clear.
    #[test]
    fn acknowledgment_rejects_changed_attempt_root_and_state() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        use storage_runtime::incident::{acknowledge_incident, review_incident};
        for case in 0..3 {
            let (state, frozen) = fixture()?;
            let state = Rc::new(RefCell::new(state));
            let scratch = Scratch::new()?;
            let (attempt, _) = interrupted(&scratch, state.clone(), &frozen)?;
            let root = ExecutionRoot::open(&scratch.0)?;
            let probe = probe(state.clone());
            let review = review_incident(&root, &probe)?.ok_or("review")?;
            let token = review.confirmation_token();
            let other = Scratch::new()?;
            let selected = if case == 0 { &other } else { &scratch };
            if case == 0 {
                let lock = ExecutionRoot::open(&other.0)?.try_lock_host()?;
                CheckpointStore::new(&lock).arm(&attempt)?;
            } else if case == 1 {
                let mut replacement = attempt.clone();
                replacement.plan_digest = "b".repeat(64);
                std::fs::write(
                    scratch.0.join("active-attempt.json"),
                    replacement.to_bytes()?,
                )?;
            } else {
                state.borrow_mut().current.mounts.entries.push(
                    storage_core::model::ObservedMount {
                        source: storage_core::model::MountSource::Tmpfs,
                        target: "/new".into(),
                        options: vec![],
                        context: storage_core::model::MountContext::Host,
                    },
                );
            }
            let mut policy = AckPolicy {
                root: selected.0.clone(),
                status: storage_engine::LiveCheckStatus::Satisfied,
                replace: false,
                calls: 0,
            };
            assert!(
                acknowledge_incident(
                    &ExecutionRoot::open(&selected.0)?,
                    &probe,
                    review,
                    token,
                    &mut policy
                )
                .is_err()
            );
            assert_eq!(policy.calls, 0);
            assert!(scratch.0.join("active-attempt.json").exists());
            assert!(selected.0.join("active-attempt.json").exists());
        }
        Ok(())
    }
}

/// An empty or partial graph cannot contradict measured PV count and still claim complete membership.
#[test]
fn vg_membership_requires_same_pass_count_present_pvs_and_physical_ancestry()
-> Result<(), Box<dyn Error>> {
    let _process = isolate_processes();
    use storage_core::model::{DependencyKind, MemberRole, Presence};
    use storage_engine::LiveCheckStatus;
    let (state, _) = fixture()?;
    let vg = state.lvm_vgs[0].node;
    let observed = ProbedObservation::collect(&probe(Rc::new(RefCell::new(state.clone()))))?;
    assert_eq!(observed.check_vg_membership(vg), LiveCheckStatus::Satisfied);
    for case in 0..7 {
        let mut changed = state.clone();
        let pv = *changed
            .current
            .graph
            .nodes()
            .find(|(_, node)| node.kind.kind == NodeKind::LvmPv)
            .ok_or("PV")?
            .0;
        match case {
            0 => changed.lvm_vgs[0].vg.pv_count += 1,
            1 => changed.lvm_vgs[0].vg.pv_count = 0,
            2 => {
                let edge = changed
                    .current
                    .graph
                    .dependencies()
                    .find(|edge| {
                        edge.to == vg && edge.kind == DependencyKind::MemberOf(MemberRole::LvmPv)
                    })
                    .cloned()
                    .ok_or("membership")?;
                changed.current.graph.remove_dependency(&edge);
            }
            3 | 4 => {
                let mut node = changed.current.graph.node(&pv).ok_or("PV")?.clone();
                if case == 3 {
                    node.size.identities.push(node.size.identities[0].clone());
                } else {
                    node.size.presence = Presence::Missing;
                }
                changed.current.graph.replace_existing(pv, node)?;
            }
            5 => {
                let edge = changed
                    .current
                    .graph
                    .dependencies()
                    .find(|edge| edge.to == pv)
                    .cloned()
                    .ok_or("backing")?;
                changed.current.graph.remove_dependency(&edge);
            }
            _ => changed.lvm_vgs.clear(),
        }
        let observed = ProbedObservation::collect(&probe(Rc::new(RefCell::new(changed))))?;
        assert!(
            matches!(
                observed.check_vg_membership(vg),
                LiveCheckStatus::Unknown { .. }
            ),
            "case {case}"
        );
    }
    Ok(())
}

/// Device paths/numbers can change, while geometry drift and unknown/read-only endpoints forbid resolution.
#[test]
fn physical_scope_checks_geometry_and_writable_evidence() -> Result<(), Box<dyn Error>> {
    let _process = isolate_processes();
    use storage_core::model::BlockSize;
    let (state, frozen) = fixture()?;
    let run = ExecutionRun::new(frozen.document())?;
    for case in 0..7 {
        let mut changed = state.clone();
        let disk = *changed
            .current
            .graph
            .nodes()
            .find(|(_, node)| node.kind.kind == NodeKind::Disk)
            .ok_or("disk")?
            .0;
        let mut node = changed.current.graph.node(&disk).ok_or("disk")?.clone();
        let block = node.size.block.as_mut().ok_or("block")?;
        match case {
            0 => {
                block.paths = vec!["/dev/renamed-fixture".into()];
                block.devno.minor += 16;
            }
            1 => block.read_only = Some(true),
            2 => block.read_only = None,
            3 => block.geometry = None,
            4 => {
                block
                    .geometry
                    .as_mut()
                    .ok_or("geometry")?
                    .logical_block_size = BlockSize::new(4096).ok_or("size")?
            }
            5 => node.kind.size = None,
            _ => node.size.identities = vec![ExternalId::Wwn("replacement-disk".into())],
        }
        changed.current.graph.replace_existing(disk, node)?;
        let observed = ProbedObservation::collect(&probe(Rc::new(RefCell::new(changed))))?;
        assert_eq!(
            resolve_action(&run, ActionId::new(0), observed.observation(), backend()).is_ok(),
            case == 0,
            "case {case}"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod native_lvm {
    use super::*;
    use storage_engine::{ActionRunState, LiveCheckStatus};
    use storage_provider::{LvmAction, ProviderAction};
    use storage_runtime::{
        capability::{BackendCapabilityCheck, OperationAvailability},
        checkpoint::CheckpointStore,
        coordinator::ExecutionCoordinator,
        dispatcher::BackendDispatcher,
        host::LinuxLiveHost,
        lease::ExecutionRoot,
        native_lvm::NativeLvmAdapter,
    };

    /// Native symbol support alone cannot grant admission or create an active checkpoint.
    #[test]
    fn native_adapter_requires_independent_scope_and_rejects_unsupported_operations()
    -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        let library = mock_lvm::Fixture::new(0)?;
        let backend = library.registry.backends().next().ok_or("mock backend")?;
        let adapter = NativeLvmAdapter::new(backend)?;
        let (state, frozen) = fixture()?;
        let mut run = ExecutionRun::new(frozen.document())?;
        let shared = Rc::new(RefCell::new(state));
        let observed = ProbedObservation::collect(&probe(shared.clone()))?;
        let action = &run.context().plan.actions()[0].operation;
        assert_eq!(
            adapter.availability(action, observed.observation()),
            OperationAvailability::Available(backend.manifest().id.clone())
        );
        assert!(matches!(
            adapter.availability(
                &ProviderAction::Lvm(LvmAction::SetActivation {
                    target: shared.borrow().lvm_vgs[0].node,
                    active: false,
                }),
                observed.observation()
            ),
            OperationAvailability::Unavailable(_)
        ));
        let resolved = resolve_action(
            &run,
            ActionId::new(0),
            observed.observation(),
            adapter.backend_id().clone(),
        )?;
        assert!(matches!(
            adapter.check_scope(&run.context(), &resolved, observed.observation()),
            LiveCheckStatus::Unknown { .. }
        ));
        let scratch = admission::Scratch::new()?;
        let root = ExecutionRoot::open(&scratch.0)?;
        let authorization = admission::authorization(&run)?;
        let approval_calls = authorization.calls.clone();
        let host = LinuxLiveHost::new(probe(shared), adapter, authorization);
        let mut coordinator =
            ExecutionCoordinator::new(ExecutionRoot::open(&scratch.0)?, host, adapter, &run);
        assert_eq!(
            coordinator.execute_action(&mut run, ActionId::new(0), None)?,
            ActionRunState::Checking
        );
        assert_eq!(approval_calls.get(), 0);
        assert_eq!(backend.probe_lvm()?.vgs[0].free, 8192);
        let lock = root.try_lock_host()?;
        assert!(CheckpointStore::new(&lock).inspect()?.is_none());
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod native_execution {
    use super::*;
    use std::{cell::Cell, fs, path::PathBuf};
    use storage_core::model::{
        Dependency, DependencyKind, LvmLv, LvmLvKind, Node, NodeFacts, NodeSpec, Presence,
    };
    use storage_engine::{ActionRunState, ExecutionRunError, LiveCheckStatus, LiveObservation};
    use storage_provider::{ProviderAction, RegisteredBackend};
    use storage_runtime::{
        authorization::{ApprovalRequest, AuthorizationContext},
        capability::{BackendCapabilityCheck, OperationAvailability},
        checkpoint::{ActiveAttempt, plan_digest},
        coordinator::{CoordinatorError, ExecutionCoordinator},
        dispatcher::{BackendDispatcher, DispatchError, ExecutionReceipt},
        host::LinuxLiveHost,
        lease::ExecutionRoot,
        native_lvm::NativeLvmAdapter,
        targets::ResolvedAction,
    };

    /// Native LVM reads over the mock library, combined with explicitly synthetic physical backing.
    struct NativeProvider<'backend> {
        /// In-memory mock registration, never the installed system plugin.
        backend: &'backend RegisteredBackend,
        /// Fixed physical facts and VG node identity, independent of the plan's predicted outputs.
        template: ProbeSnapshot,
    }

    impl NativeProvider<'_> {
        /// Converts fresh native records to graph facts without consulting the execution plan.
        fn snapshot(&self) -> Result<ProbeSnapshot, Box<dyn Error>> {
            let native = self.backend.probe_lvm()?;
            let vg = native.vgs.first().ok_or("mock VG missing")?;
            let mut state = self.template.clone();
            let vg_id = state.lvm_vgs[0].node;
            let mut node = state
                .current
                .graph
                .node(&vg_id)
                .ok_or("template VG")?
                .clone();
            node.kind = NodeSpec {
                kind: NodeKind::LvmVg {
                    name: vg.name.clone(),
                    extent_size: Some(Bytes::new(vg.extent_size)),
                },
                size: Some(Bytes::new(vg.size)),
            };
            node.size.identities = vec![ExternalId::LvmVgUuid(vg.uuid.clone())];
            state.current.graph.replace_existing(vg_id, node)?;
            state.lvm_vgs[0].vg = vg.clone();
            let (pv_id, mut pv) = state
                .current
                .graph
                .nodes()
                .find(|(_, node)| matches!(node.kind.kind, NodeKind::LvmPv))
                .map(|(id, node)| (*id, node.clone()))
                .ok_or("template PV")?;
            let record = native.pvs.first().ok_or("native PV")?;
            pv.kind.size = Some(Bytes::new(record.size));
            pv.size.identities = vec![ExternalId::LvmPvUuid(record.uuid.clone())];
            state.current.graph.replace_existing(pv_id, pv)?;
            for lv in native.lvs {
                let active = lv.activation();
                if lv.vg_name != vg.name || lv.segment_type.as_deref() != Some("linear") {
                    return Err("unsupported fixture LV".into());
                }
                let id = NodeId::new();
                state.current.graph.insert_new(
                    id,
                    Node {
                        kind: NodeSpec {
                            kind: NodeKind::LvmLv(LvmLv {
                                name: lv.name,
                                kind: LvmLvKind::Linear,
                            }),
                            size: Some(Bytes::new(lv.size)),
                        },
                        size: NodeFacts {
                            presence: Presence::Present,
                            lvm_active: active,
                            // Private fixture geometry is independent of the plan and has no host endpoint.
                            block: if active == Some(true) {
                                self.template
                                    .current
                                    .graph
                                    .nodes()
                                    .find_map(|(_, node)| {
                                        if matches!(node.kind.kind, NodeKind::Disk) {
                                            node.size.block.clone()
                                        } else {
                                            None
                                        }
                                    })
                                    .map(|mut block| {
                                        block.paths = vec!["/dev/mock-lv".into()];
                                        block.devno = storage_core::model::DeviceNumber {
                                            major: 253,
                                            minor: 17,
                                        };
                                        block
                                    })
                            } else {
                                None
                            },
                            identities: vec![ExternalId::LvmLvUuid(lv.uuid)],
                            ..NodeFacts::default()
                        },
                    },
                )?;
                state.current.graph.insert_dependency(Dependency {
                    from: vg_id,
                    to: id,
                    kind: DependencyKind::Provides,
                });
            }
            // Recorded existing filesystems get fresh transient IDs; UUIDs and all relationships remain intact.
            let filesystems: Vec<_> = state
                .current
                .graph
                .nodes()
                .filter(|(_, node)| matches!(node.kind.kind, NodeKind::Filesystem { .. }))
                .map(|(id, _)| *id)
                .collect();
            for id in filesystems {
                state = serde_json::from_str(&serde_json::to_string(&state)?.replace(
                    &id.as_uuid().to_string(),
                    &NodeId::new().as_uuid().to_string(),
                ))?;
            }
            for mount in &mut state.current.mounts.entries {
                mount.options.reverse();
            }
            Ok(state)
        }
    }

    impl StateProvider for NativeProvider<'_> {
        /// Distinguishes this mock contribution from installed native providers.
        fn id(&self) -> &str {
            "native-lvm-abi-fixture"
        }
        /// The mock owns its entire isolated topology for this test.
        fn responsibility(&self) -> &str {
            "fixture.native_topology"
        }
        /// This test adapter calls a loaded mock library.
        fn connection_kind(&self) -> ProviderConnectionKind {
            ProviderConnectionKind::BuiltIn
        }
        /// Reads the C fixture anew on every host probe, including after native errors.
        fn probe(&self) -> Result<ProviderState, ProviderProbeError> {
            let snapshot = self.snapshot().map_err(|error| {
                ProviderProbeError::new("fixture.native_probe", error.to_string())
            })?;
            Ok(ProviderState {
                graph: snapshot.current.graph,
                mounts: snapshot.current.mounts.entries,
                lvm_vgs: snapshot.lvm_vgs,
                environment: Some(snapshot.current.environment),
                diagnostics: vec![],
            })
        }
    }

    /// Synthetic scope policy is confined to this disk-free fixture and never exported as production evidence.
    struct MockScope<'backend>(NativeLvmAdapter<'backend>, bool);
    impl BackendCapabilityCheck for MockScope<'_> {
        /// Final scope is a separate explicit proof for this private-memory fixture.
        fn check_final_scope(
            &self,
            _context: &storage_engine::ExecutionContext<'_>,
            _observed: &LiveObservation,
        ) -> LiveCheckStatus {
            if self.1 {
                LiveCheckStatus::Satisfied
            } else {
                LiveCheckStatus::Unknown {
                    required_evidence: "fixture final coverage missing".into(),
                }
            }
        }

        /// Operation support still comes from the actual typed adapter and mock C technology query.
        fn availability(
            &self,
            operation: &ProviderAction,
            observed: &LiveObservation,
        ) -> OperationAvailability {
            self.0.availability(operation, observed)
        }
        /// The fixture consists only of private memory; it has no external writers or namespaces.
        fn check_scope(
            &self,
            _context: &storage_engine::ExecutionContext<'_>,
            _action: &ResolvedAction,
            _observed: &LiveObservation,
        ) -> LiveCheckStatus {
            LiveCheckStatus::Satisfied
        }
    }

    /// Explicit fixture approval for one immutable document, run and UUID-qualified target.
    struct MockApproval {
        /// Exact test execution attempt.
        run_id: uuid::Uuid,
        /// Trusted canonical document digest computed before dispatch.
        digest: String,
    }
    impl AuthorizationContext for MockApproval {
        /// No blanket root/yes grant: the mock request must still match this fixture's intent.
        fn check(&mut self, request: &ApprovalRequest) -> LiveCheckStatus {
            if request.run_id == self.run_id
                && request.plan_digest == self.digest
                && request.backend.as_str() == "mock-lvm"
                && ((request.action_id == ActionId::new(0) && request.targets.len() == 1)
                    || (request.action_id == ActionId::new(1)
                        && request.targets.len() == 2
                        && request.targets[1].external_ids
                            == vec![ExternalId::LvmLvUuid("new-uuid".into())]))
                && request.targets[0].external_ids == vec![ExternalId::LvmVgUuid("vg-uuid".into())]
            {
                LiveCheckStatus::Satisfied
            } else {
                LiveCheckStatus::Violated {
                    reason: "fixture approval mismatch".into(),
                }
            }
        }
    }

    /// Observes the durable checkpoint and held lock immediately before delegating to the native adapter.
    struct CheckedDispatcher<'backend> {
        /// Exact native adapter selected during admission.
        native: NativeLvmAdapter<'backend>,
        /// Isolated execution root whose marker must already exist.
        root: PathBuf,
        /// Counts native adapter entries to prove absence of retries.
        calls: Rc<Cell<usize>>,
    }
    impl BackendDispatcher for CheckedDispatcher<'_> {
        /// Uses the same registered backend throughout the attempt.
        fn backend_id(&self) -> &BackendId {
            self.native.backend_id()
        }
        /// Asserts checkpoint ordering before crossing the real Rust-to-C call boundary.
        fn execute(
            &mut self,
            action: &ResolvedAction,
            progress: &mut dyn FnMut(String) -> Result<(), ExecutionRunError>,
        ) -> Result<ExecutionReceipt, DispatchError> {
            let attempt = ActiveAttempt::from_bytes(
                &fs::read(self.root.join("active-attempt.json"))
                    .unwrap_or_else(|error| panic!("checkpoint before native call: {error}")),
            )
            .unwrap_or_else(|error| panic!("checkpoint parse: {error}"));
            assert_eq!(attempt.run_id, action.run_id());
            assert_eq!(attempt.action_id, action.action_id());
            assert_eq!(&attempt.backend_id, action.backend());
            assert_eq!(&attempt.action, action.operation());
            assert_eq!(attempt.targets, action.fingerprints());
            let root = ExecutionRoot::open(&self.root)
                .unwrap_or_else(|error| panic!("fixture root: {error}"));
            assert!(root.try_lock_host().is_err());
            self.calls.set(self.calls.get() + 1);
            self.native.execute(action, progress)
        }
    }

    /// The full path verifies a native success and retains incidents for native/query/result failures.
    #[test]
    fn native_execution_observes_checkpoint_and_never_retries_uncertain_effects()
    -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        for scenario in [0, 7, 8, 9, 14, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29] {
            let library = mock_lvm::Fixture::new(scenario)?;
            let backend = library.registry.backends().next().ok_or("mock backend")?;
            let (template, _) = fixture()?;
            let provider = NativeProvider { backend, template };
            let initial = provider.snapshot()?;
            let baseline =
                HistoricalBaseline::from_lvm_observations(initial.current, &initial.lvm_vgs)?;
            let mut session = PlanSession::new(baseline)?;
            session.add_lv(
                session.revision(),
                initial.lvm_vgs[0].node,
                "data",
                Bytes::new(4096),
            )?;
            let frozen = session.freeze(session.revision())?;
            let mut run = ExecutionRun::new(frozen.document())?;
            let adapter = NativeLvmAdapter::new(backend)?;
            let scratch = admission::Scratch::new()?;
            let calls = Rc::new(Cell::new(0));
            let dispatcher = CheckedDispatcher {
                native: adapter,
                root: scratch.0.clone(),
                calls: calls.clone(),
            };
            let host = LinuxLiveHost::new(
                LinuxProbe::from_providers(vec![Box::new(provider)]),
                MockScope(adapter, true),
                MockApproval {
                    run_id: run.run_id(),
                    digest: plan_digest(run.document())?,
                },
            );
            let mut coordinator =
                ExecutionCoordinator::new(ExecutionRoot::open(&scratch.0)?, host, dispatcher, &run);
            let result = coordinator.execute_action(&mut run, ActionId::new(0), None);
            assert_eq!(calls.get(), 1, "scenario {scenario}: {result:?}");
            if scenario == 0 || scenario >= 19 {
                assert_eq!(result?, ActionRunState::Verified);
                assert_eq!(coordinator.bindings().len(), 1);
                assert!(!scratch.0.join("active-attempt.json").exists());
                let activated = coordinator.execute_action(&mut run, ActionId::new(1), None);
                if scenario == 0 {
                    assert_eq!(activated?, ActionRunState::Verified);
                    assert_eq!(calls.get(), 2);
                    assert!(!scratch.0.join("active-attempt.json").exists());
                    assert!(!run.is_verified());
                } else if scenario >= 28 {
                    assert!(activated.is_err() || activated? != ActionRunState::Verified);
                    assert_eq!(calls.get(), 1);
                    assert!(!scratch.0.join("active-attempt.json").exists());
                } else {
                    assert_eq!(calls.get(), 2, "activation {scenario}: {activated:?}");
                    assert!(activated.is_err() || activated? == ActionRunState::OutcomeUnknown);
                    assert_eq!(run.state(ActionId::new(1))?, ActionRunState::OutcomeUnknown);
                    assert!(scratch.0.join("active-attempt.json").exists());
                    assert!(matches!(
                        coordinator.execute_action(&mut run, ActionId::new(1), None),
                        Err(CoordinatorError::Stopped)
                    ));
                    assert_eq!(calls.get(), 2);
                }
            } else {
                assert!(result.is_err() || result? != ActionRunState::Verified);
                assert_eq!(run.state(ActionId::new(0))?, ActionRunState::OutcomeUnknown);
                assert!(coordinator.bindings().is_empty());
                assert!(scratch.0.join("active-attempt.json").exists());
                assert!(matches!(
                    coordinator.execute_action(&mut run, ActionId::new(0), None),
                    Err(CoordinatorError::Stopped)
                ));
                assert_eq!(calls.get(), 1);
            }
        }
        Ok(())
    }
    /// Whole native runs need final evidence; unavailable successors veto even the first native write.
    #[test]
    fn native_dag_summary_requires_independent_final_success() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        for case in 0..4 {
            let scenario = match case {
                2 => 29,
                3 => 19,
                _ => 0,
            };
            let library = mock_lvm::Fixture::new(scenario)?;
            let backend = library.registry.backends().next().ok_or("mock backend")?;
            let (template, _) = fixture()?;
            let provider = NativeProvider { backend, template };
            let initial = provider.snapshot()?;
            let baseline =
                HistoricalBaseline::from_lvm_observations(initial.current, &initial.lvm_vgs)?;
            let mut session = PlanSession::new(baseline)?;
            session.add_lv(
                session.revision(),
                initial.lvm_vgs[0].node,
                "data",
                Bytes::new(4096),
            )?;
            let frozen = session.freeze(session.revision())?;
            let mut run = ExecutionRun::new(frozen.document())?;
            let adapter = NativeLvmAdapter::new(backend)?;
            let scratch = admission::Scratch::new()?;
            let calls = Rc::new(Cell::new(0));
            let dispatcher = CheckedDispatcher {
                native: adapter,
                root: scratch.0.clone(),
                calls: calls.clone(),
            };
            let host = LinuxLiveHost::new(
                LinuxProbe::from_providers(vec![Box::new(provider)]),
                MockScope(adapter, case != 1),
                MockApproval {
                    run_id: run.run_id(),
                    digest: plan_digest(run.document())?,
                },
            );
            let mut coordinator =
                ExecutionCoordinator::new(ExecutionRoot::open(&scratch.0)?, host, dispatcher, &run);
            let result = coordinator.execute_run(&mut run);
            if case == 2 {
                assert!(matches!(result, Err(CoordinatorError::PlanUnavailable(_))));
                assert_eq!(calls.get(), 0);
                assert!(backend.probe_lvm()?.lvs.is_empty());
            } else {
                let summary = result?;
                assert_eq!(summary.verified, case == 0);
                assert_eq!(calls.get(), 2);
                assert_eq!(run.is_verified(), case == 0);
                if case == 3 {
                    assert_eq!(summary.stopped_action, Some(ActionId::new(1)));
                    assert!(scratch.0.join("active-attempt.json").exists());
                    assert!(matches!(
                        coordinator.execute_run(&mut run),
                        Err(CoordinatorError::Stopped)
                    ));
                } else {
                    assert!(summary.stopped_action.is_none());
                    assert!(run.goal_verification().is_some());
                    assert_eq!(run.state(ActionId::new(0))?, ActionRunState::Verified);
                    assert_eq!(run.state(ActionId::new(1))?, ActionRunState::Verified);
                    if case == 1 {
                        assert!(!coordinator.execute_run(&mut run)?.verified);
                        assert_eq!(calls.get(), 2);
                        assert_eq!(backend.probe_lvm()?.lvs[0].copy_percent, 1);
                    }
                }
            }
            if case != 3 {
                assert!(!scratch.0.join("active-attempt.json").exists());
            }
        }
        Ok(())
    }
    /// Fault injection occurs only on a later probe after activation was already observed once.
    struct MountedNativeProvider<'backend> {
        /// Native LV reads plus recorded independent physical/filesystem observations.
        inner: NativeProvider<'backend>,
        /// Final fault: zero none, one options, two overmount, three filesystem UUID replacement.
        fault: u8,
        /// Distinguishes the action post-probe from the independent final probe.
        active_seen: Cell<bool>,
    }
    impl StateProvider for MountedNativeProvider<'_> {
        /// Keeps the fixture distinct from installed providers.
        fn id(&self) -> &str {
            "mounted-native-fixture"
        }
        /// Owns the complete isolated topology in this test only.
        fn responsibility(&self) -> &str {
            "fixture.mounted_native"
        }
        /// No external process or disk device is involved in this provider.
        fn connection_kind(&self) -> ProviderConnectionKind {
            ProviderConnectionKind::BuiltIn
        }
        /// Publishes fresh native state, optionally injecting drift only after both actions were Verified.
        fn probe(&self) -> Result<ProviderState, ProviderProbeError> {
            let mut state = self.inner.probe()?;
            let active = state.graph.nodes().any(|(_, node)| {
                matches!(node.kind.kind, NodeKind::LvmLv(_)) && node.size.lvm_active == Some(true)
            });
            if active && self.active_seen.replace(true) {
                // Equivalent option order in a later publication must not invalidate a mount goal.
                for mount in &mut state.mounts {
                    mount.options.reverse();
                }
                match self.fault {
                    1 => state.mounts[0].options = vec!["ro".into(), "nodev".into()],
                    2 => {
                        let mut overmount = state.mounts[0].clone();
                        overmount.source = storage_core::model::MountSource::Tmpfs;
                        state.mounts.push(overmount);
                    }
                    3 => {
                        let (id, mut node) = state
                            .graph
                            .nodes()
                            .find(|(_, node)| matches!(node.kind.kind, NodeKind::Filesystem { .. }))
                            .map(|(id, node)| (*id, node.clone()))
                            .ok_or_else(|| {
                                ProviderProbeError::new("fixture.fs", "missing filesystem")
                            })?;
                        node.size.identities = vec![ExternalId::Filesystem {
                            fs_type: "ext4".into(),
                            value: "replacement".into(),
                        }];
                        state.graph.replace_existing(id, node).map_err(|error| {
                            ProviderProbeError::new("fixture.fs", error.to_string())
                        })?;
                    }
                    _ => {}
                }
            }
            Ok(state)
        }
    }

    /// Existing mounted filesystems survive transient IDs; final drift cannot be hidden by earlier Verified actions.
    #[test]
    fn native_run_rechecks_existing_mounts_after_verified_actions() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        use storage_core::model::{FilesystemKind, MountContext, MountSource, ObservedMount};
        for fault in 0..4 {
            let library = mock_lvm::Fixture::new(0)?;
            let backend = library.registry.backends().next().ok_or("mock backend")?;
            let (mut template, _) = fixture()?;
            let original = template
                .current
                .graph
                .nodes()
                .find(|(_, node)| matches!(node.kind.kind, NodeKind::Disk))
                .map(|(_, node)| node.clone())
                .ok_or("disk")?;
            let disk = NodeId::new();
            let fs = NodeId::new();
            let mut disk_node = original.clone();
            disk_node.size.identities = vec![ExternalId::Wwn("existing-root-disk".into())];
            template.current.graph.insert_new(disk, disk_node)?;
            let mut filesystem = original;
            filesystem.kind.kind = NodeKind::Filesystem {
                kind: FilesystemKind::Ext4,
                label: Some("existing".into()),
            };
            filesystem.size.identities = vec![ExternalId::Filesystem {
                fs_type: "ext4".into(),
                value: "existing-fs-uuid".into(),
            }];
            filesystem.size.block = None;
            template.current.graph.insert_new(fs, filesystem)?;
            template.current.graph.insert_dependency(Dependency {
                from: disk,
                to: fs,
                kind: DependencyKind::Backs,
            });
            template.current.mounts.entries.push(ObservedMount {
                source: MountSource::Filesystem(fs),
                target: "/existing".into(),
                options: vec!["rw".into(), "nodev".into()],
                context: MountContext::Host,
            });
            let inner = NativeProvider { backend, template };
            let initial = inner.snapshot()?;
            let baseline =
                HistoricalBaseline::from_lvm_observations(initial.current, &initial.lvm_vgs)?;
            let mut session = PlanSession::new(baseline)?;
            session.add_lv(
                session.revision(),
                initial.lvm_vgs[0].node,
                "data",
                Bytes::new(4096),
            )?;
            let frozen = session.freeze(session.revision())?;
            let mut run = ExecutionRun::new(frozen.document())?;
            let adapter = NativeLvmAdapter::new(backend)?;
            let scratch = admission::Scratch::new()?;
            let calls = Rc::new(Cell::new(0));
            let dispatcher = CheckedDispatcher {
                native: adapter,
                root: scratch.0.clone(),
                calls: calls.clone(),
            };
            let provider = MountedNativeProvider {
                inner,
                fault,
                active_seen: Cell::new(false),
            };
            let host = LinuxLiveHost::new(
                LinuxProbe::from_providers(vec![Box::new(provider)]),
                MockScope(adapter, true),
                MockApproval {
                    run_id: run.run_id(),
                    digest: plan_digest(run.document())?,
                },
            );
            let mut coordinator =
                ExecutionCoordinator::new(ExecutionRoot::open(&scratch.0)?, host, dispatcher, &run);
            let summary = coordinator.execute_run(&mut run)?;
            assert_eq!(summary.verified, fault == 0, "fault {fault}");
            assert_eq!(calls.get(), 2);
            assert_eq!(run.state(ActionId::new(0))?, ActionRunState::Verified);
            assert_eq!(run.state(ActionId::new(1))?, ActionRunState::Verified);
            assert!(summary.stopped_action.is_none());
            assert!(!scratch.0.join("active-attempt.json").exists());
            let report = run.goal_verification().ok_or("final report")?;
            if fault != 0 {
                assert!(
                    report
                        .checks
                        .iter()
                        .any(|check| check.status != LiveCheckStatus::Satisfied)
                );
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod activation {
    use super::*;
    use storage_engine::ActionRunState;
    use storage_runtime::{
        coordinator::ExecutionCoordinator, lease::ExecutionRoot,
        targets::resolve_action_with_bindings,
    };

    /// A verified LV survives transient IDs; replacement, unknown activation and read-only results fail closed.
    #[test]
    fn activation_uses_verified_output_and_checks_actual_interface() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        for case in 0..6 {
            let (state, frozen) = fixture()?;
            let state = Rc::new(RefCell::new(state));
            let scratch = admission::Scratch::new()?;
            let mut run = ExecutionRun::new(frozen.document())?;
            let mut fake = coordination::dispatcher(&scratch, state.clone());
            fake.created_uuid = Some("created-lv".into());
            fake.result_case = match case {
                3 => 8,
                4 => 9,
                _ => 0,
            };
            let calls = fake.calls.clone();
            let mut coordinator = ExecutionCoordinator::new(
                ExecutionRoot::open(&scratch.0)?,
                coordination::host(state.clone(), &run)?,
                fake,
                &run,
            );
            assert_eq!(
                coordinator.execute_action(&mut run, ActionId::new(0), None)?,
                ActionRunState::Verified
            );
            let old = state
                .borrow()
                .current
                .graph
                .nodes()
                .find(|(_, node)| matches!(node.kind.kind, NodeKind::LvmLv(_)))
                .map(|(id, _)| *id)
                .ok_or("created LV")?;
            if case == 1 {
                let remapped = serde_json::to_string(&*state.borrow())?.replace(
                    &old.as_uuid().to_string(),
                    &NodeId::new().as_uuid().to_string(),
                );
                *state.borrow_mut() = serde_json::from_str(&remapped)?;
            } else if case == 2 {
                let mut node = state.borrow().current.graph.node(&old).ok_or("LV")?.clone();
                node.size.identities = vec![ExternalId::LvmLvUuid("replacement".into())];
                state
                    .borrow_mut()
                    .current
                    .graph
                    .replace_existing(old, node)?;
            }
            if case == 5 {
                let mut node = state.borrow().current.graph.node(&old).ok_or("LV")?.clone();
                if let NodeKind::LvmLv(lv) = &mut node.kind.kind {
                    lv.name = "foreign".into();
                }
                node.size.identities = vec![ExternalId::LvmLvUuid("foreign-lv".into())];
                let id = NodeId::new();
                let vg = state.borrow().lvm_vgs[0].node;
                state.borrow_mut().current.graph.insert_new(id, node)?;
                state.borrow_mut().current.graph.insert_dependency(
                    storage_core::model::Dependency {
                        from: vg,
                        to: id,
                        kind: storage_core::model::DependencyKind::Provides,
                    },
                );
            }
            let result = coordinator.execute_action(&mut run, ActionId::new(1), None);
            if case <= 1 {
                assert_eq!(result?, ActionRunState::Verified);
                assert_eq!(calls.get(), 2);
                assert!(!scratch.0.join("active-attempt.json").exists());
                assert!(!run.is_verified());
            } else if case == 2 || case == 5 {
                assert!(result.is_err() || result? != ActionRunState::Verified);
                assert_eq!(calls.get(), 1);
                assert!(!scratch.0.join("active-attempt.json").exists());
            } else {
                assert_eq!(calls.get(), 2, "case {case}: {result:?}");
                assert!(result.is_err() || result? == ActionRunState::OutcomeUnknown);
                assert_eq!(run.state(ActionId::new(1))?, ActionRunState::OutcomeUnknown);
                assert!(scratch.0.join("active-attempt.json").exists());
            }
        }
        // An identically named planned output is unusable without a verified binding from this run.
        let (state, frozen) = fixture()?;
        let run = ExecutionRun::new(frozen.document())?;
        let observed = ProbedObservation::collect(&probe(Rc::new(RefCell::new(state))))?;
        assert!(
            resolve_action_with_bindings(
                &run,
                ActionId::new(1),
                observed.observation(),
                backend(),
                &storage_runtime::bindings::OutputBindings::new(&run)
            )
            .is_err()
        );
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod final_host {
    use super::*;
    use storage_engine::{ExecutionContext, LiveCheckStatus, LiveObservation};
    use storage_runtime::{
        bindings::OutputBindings,
        capability::{BackendCapabilityCheck, OperationAvailability},
        host::{HostError, LinuxLiveHost, RuntimeHost},
        lease::ExecutionRoot,
    };

    /// Explicit synthetic final policy, independent from per-operation availability/admission.
    struct Policy {
        /// Whether this final publication covers the private fixture completely.
        coverage: LiveCheckStatus,
        /// Independently selected preservation result for the fixture.
        preservation: LiveCheckStatus,
    }
    impl BackendCapabilityCheck for Policy {
        /// This test contains no actions and never allows any write operation.
        fn availability(
            &self,
            _operation: &storage_provider::ProviderAction,
            _observed: &LiveObservation,
        ) -> OperationAvailability {
            OperationAvailability::Unavailable("read-only final fixture".into())
        }
        /// Fixture final coverage is deliberately separate from action admission scope.
        fn check_final_scope(
            &self,
            _context: &ExecutionContext<'_>,
            _observed: &LiveObservation,
        ) -> LiveCheckStatus {
            self.coverage.clone()
        }
        /// Even positive final coverage cannot replace a negative preservation result.
        fn check_preservation(
            &self,
            _context: &ExecutionContext<'_>,
            _observed: &LiveObservation,
        ) -> LiveCheckStatus {
            self.preservation.clone()
        }
    }

    /// Final success requires complete fresh coverage and independent positive preservation.
    #[test]
    fn final_host_requires_fresh_coverage_and_preservation() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        let (state, _) = fixture()?;
        let session = PlanSession::new(HistoricalBaseline::from_lvm_observations(
            state.current.clone(),
            &state.lvm_vgs,
        )?)?;
        let frozen = session.freeze(session.revision())?;
        for case in 0..4 {
            let mut run = ExecutionRun::new(frozen.document())?;
            let policy = Policy {
                coverage: if case == 0 {
                    LiveCheckStatus::Unknown {
                        required_evidence: "coverage".into(),
                    }
                } else {
                    LiveCheckStatus::Satisfied
                },
                preservation: if case == 1 {
                    LiveCheckStatus::Violated {
                        reason: "protected contents changed".into(),
                    }
                } else {
                    LiveCheckStatus::Satisfied
                },
            };
            let mut host = LinuxLiveHost::new(
                probe(Rc::new(RefCell::new(state.clone()))),
                policy,
                admission::authorization(&run)?,
            );
            let observed = host.probe()?;
            if case == 3 {
                host.probe()?;
            }
            let scratch = admission::Scratch::new()?;
            let root = ExecutionRoot::open(&scratch.0)?;
            let lock = root.try_lock_host()?;
            let report = host.verify_goal(&run, &observed, &OutputBindings::new(&run), &lock);
            if case == 3 {
                assert!(matches!(report, Err(HostError::ObservationMismatch)));
            } else {
                assert_eq!(run.record_goal_verification(report?)?, case == 2);
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod run_coordinator {
    use super::*;
    use std::path::PathBuf;
    use storage_engine::{ActionRunState, GoalVerificationReport};
    use storage_runtime::{
        bindings::OutputBindings,
        coordinator::{CoordinatorError, ExecutionCoordinator},
        dispatcher::{BackendDispatcher, DispatchError, ExecutionReceipt},
        host::{AdmissionDecision, HostError, RuntimeHost},
        lease::{ExecutionLease, ExecutionRoot, HostExecutionLock},
    };

    /// Forces event eviction while the ordinary synthetic dispatcher/checkpoint path remains active.
    struct BurstDispatcher {
        /// Existing mutation fixture, including its checkpoint and identity assertions.
        inner: coordination::FakeDispatcher,
        /// Additional events delivered before the backend call returns.
        progress: usize,
    }
    impl BackendDispatcher for BurstDispatcher {
        /// Keeps the existing concrete backend route.
        fn backend_id(&self) -> &BackendId {
            self.inner.backend_id()
        }
        /// Emits a bounded burst after the synthetic effect and before completion verification.
        fn execute(
            &mut self,
            action: &storage_runtime::targets::ResolvedAction,
            progress: &mut dyn FnMut(String) -> Result<(), storage_engine::ExecutionRunError>,
        ) -> Result<ExecutionReceipt, DispatchError> {
            let receipt = self.inner.execute(action, progress)?;
            for _ in 0..self.progress {
                progress("fixture event burst".into())?;
            }
            Ok(receipt)
        }
    }

    /// Verifies continuous cooperative exclusion on every probe, including between actions and at final verification.
    struct LockedHost<H> {
        /// Real runtime host over recorded private fixture state.
        inner: H,
        /// Root shared by the coordinator and the competing lock attempt.
        root: PathBuf,
    }
    impl<H: RuntimeHost> RuntimeHost for LockedHost<H> {
        /// Keeps whole-plan readiness checks on the actual host implementation.
        fn check_plan(
            &mut self,
            run: &ExecutionRun,
            observed: &ProbedObservation,
            dispatcher: &dyn storage_runtime::dispatcher::BackendDispatcher,
            lock: &HostExecutionLock,
        ) -> Result<storage_engine::LiveCheckStatus, HostError> {
            self.inner.check_plan(run, observed, dispatcher, lock)
        }
        /// Every entry must see the run lock already held, not merely the preceding action's lease.
        fn probe(&mut self) -> Result<ProbedObservation, HostError> {
            let root = ExecutionRoot::open(&self.root)
                .unwrap_or_else(|error| panic!("fixture root: {error}"));
            assert!(root.try_lock_host().is_err());
            self.inner.probe()
        }
        /// Reuses the actual host admission and full binding checks.
        fn check_admission(
            &mut self,
            run: &ExecutionRun,
            action: ActionId,
            observed: &ProbedObservation,
            bindings: &OutputBindings,
            lease: &ExecutionLease,
        ) -> Result<AdmissionDecision, HostError> {
            self.inner
                .check_admission(run, action, observed, bindings, lease)
        }
        /// Reuses independent host final verification under the continuously retained run guard.
        fn verify_goal(
            &mut self,
            run: &ExecutionRun,
            observed: &ProbedObservation,
            bindings: &OutputBindings,
            lock: &HostExecutionLock,
        ) -> Result<GoalVerificationReport, HostError> {
            self.inner.verify_goal(run, observed, bindings, lock)
        }
    }

    /// Passive snapshots expose durable boundaries without fabricating final success or event IDs.
    #[test]
    fn updates_expose_armed_and_verified_before_clear() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        let (state, frozen) = fixture()?;
        let state = Rc::new(RefCell::new(state));
        let scratch = admission::Scratch::new()?;
        let mut run = ExecutionRun::new(frozen.document())?;
        let mut fake = coordination::dispatcher(&scratch, state.clone());
        fake.created_uuid = Some("created-lv".into());
        let calls = fake.calls.clone();
        let mut coordinator = ExecutionCoordinator::new(
            ExecutionRoot::open(&scratch.0)?,
            coordination::host(state, &run)?,
            fake,
            &run,
        );
        let mut phases = Vec::new();
        let summary = coordinator.execute_run_with_updates(&mut run, &mut |run| {
            let phase = run
                .state(ActionId::new(0))
                .unwrap_or_else(|error| panic!("state: {error}"));
            if scratch.0.join("active-attempt.json").exists() && calls.get() <= 1 {
                assert!(!run.is_verified());
                assert!(
                    ExecutionRoot::open(&scratch.0)
                        .unwrap_or_else(|error| panic!("root: {error}"))
                        .try_lock_host()
                        .is_err()
                );
                phases.push(phase);
            }
        })?;
        assert!(summary.verified);
        for phase in [
            ActionRunState::Admitted,
            ActionRunState::Running,
            ActionRunState::Verifying,
            ActionRunState::Verified,
        ] {
            assert!(phases.contains(&phase), "missing {phase:?}: {phases:?}");
        }
        assert_eq!(calls.get(), 2);
        assert_eq!(std::fs::read_dir(&scratch.0)?.count(), 1);
        Ok(())
    }

    /// A complete DAG gets an independent final publication; unknown/failed actions stop successors.
    #[test]
    fn executes_dag_under_one_lock_and_verifies_final_goal() -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        for case in 0..3 {
            let (state, frozen) = fixture()?;
            let state = Rc::new(RefCell::new(state));
            let scratch = admission::Scratch::new()?;
            let mut run = ExecutionRun::new(frozen.document())?;
            let mut fake = coordination::dispatcher(&scratch, state.clone());
            fake.created_uuid = Some("created-lv".into());
            if case == 1 {
                fake.created_uuid = None;
            }
            if case == 2 {
                fake.result_case = 8;
            }
            let calls = fake.calls.clone();
            let host = LockedHost {
                inner: coordination::host(state, &run)?,
                root: scratch.0.clone(),
            };
            let dispatcher = BurstDispatcher {
                inner: fake,
                progress: if case == 0 { 1100 } else { 0 },
            };
            let mut coordinator =
                ExecutionCoordinator::new(ExecutionRoot::open(&scratch.0)?, host, dispatcher, &run);
            let mut delivered = Vec::new();
            let summary = coordinator.execute_run_with_events(&mut run, &mut |event| {
                if matches!(event.kind, storage_engine::RunEventKind::Progress { .. }) {
                    // A replay after return cannot observe the success case's armed checkpoint.
                    assert!(scratch.0.join("active-attempt.json").exists());
                    assert!(
                        ExecutionRoot::open(&scratch.0)
                            .unwrap_or_else(|error| panic!("observer root: {error}"))
                            .try_lock_host()
                            .is_err()
                    );
                    assert!(calls.get() > 0);
                }
                delivered.push(event.clone());
            })?;
            let retained = run.events_page(0)?;
            assert_eq!(retained.gap, case == 0);
            assert_eq!(delivered.len() as u64, run.snapshot().event_sequence);
            assert_eq!(
                retained.events,
                delivered[delivered.len() - retained.events.len()..]
            );
            assert!(
                delivered.iter().any(|event| matches!(
                    event.kind,
                    storage_engine::RunEventKind::Progress { .. }
                ))
            );
            assert_eq!(summary.verified, case == 0);
            assert_eq!(summary.run_id, run.run_id());
            if case == 0 {
                assert!(summary.stopped_action.is_none());
                assert!(run.is_verified());
                let final_epoch = summary
                    .observation
                    .ok_or("final observation")?
                    .reference
                    .epoch;
                assert!(
                    final_epoch
                        > run
                            .completion(ActionId::new(1))?
                            .ok_or("completion")?
                            .observation
                            .reference
                            .epoch
                );
                assert_eq!(calls.get(), 2);
                assert!(!scratch.0.join("active-attempt.json").exists());
                assert!(matches!(
                    coordinator.execute_run(&mut run),
                    Err(CoordinatorError::AlreadyVerified)
                ));
            } else {
                let stopped = if case == 1 {
                    ActionId::new(0)
                } else {
                    ActionId::new(1)
                };
                assert_eq!(summary.stopped_action, Some(stopped));
                assert_eq!(run.state(stopped)?, ActionRunState::OutcomeUnknown);
                assert_eq!(calls.get(), if case == 1 { 1 } else { 2 });
                assert!(scratch.0.join("active-attempt.json").exists());
                assert!(matches!(
                    coordinator.execute_run(&mut run),
                    Err(CoordinatorError::Stopped)
                ));
            }
            // The duplicated per-action fd must not leave a hidden lock after the run returns.
            assert!(ExecutionRoot::open(&scratch.0)?.try_lock_host().is_ok());
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod plan_gate {
    use super::*;
    use storage_engine::{ActionRunState, LiveObservation};
    use storage_provider::{LvmAction, ProviderAction};
    use storage_runtime::{
        capability::{BackendCapabilityCheck, OperationAvailability},
        coordinator::{CoordinatorError, ExecutionCoordinator},
        host::LinuxLiveHost,
        lease::ExecutionRoot,
    };

    /// Create is available, but the later activation lacks trustworthy backend readiness.
    struct LaterUnsupported(u8);
    impl BackendCapabilityCheck for LaterUnsupported {
        /// Returns an operation-specific failure only for the successor of the first write.
        fn availability(
            &self,
            action: &ProviderAction,
            _observed: &LiveObservation,
        ) -> OperationAvailability {
            if matches!(action, ProviderAction::Lvm(LvmAction::SetActivation { .. })) {
                match self.0 {
                    0 => OperationAvailability::Unavailable("activation missing".into()),
                    1 => OperationAvailability::Unknown("activation readiness unknown".into()),
                    _ => OperationAvailability::Available(
                        BackendId::new("other-backend").unwrap_or_else(|| unreachable!()),
                    ),
                }
            } else {
                OperationAvailability::Available(backend())
            }
        }
    }

    /// Known later unsupported work and historical protection violations cannot leave a partial write.
    #[test]
    fn rejects_late_unsupported_action_and_protection_before_any_dispatch()
    -> Result<(), Box<dyn Error>> {
        let _process = isolate_processes();
        for case in 0..4 {
            let (state, frozen) = fixture()?;
            let mut document = frozen.document().clone();
            if case == 3 {
                document.protected_scope.nodes.push(state.lvm_vgs[0].node);
                assert!(ExecutionRun::new(&document).is_err());
                continue;
            }
            let mut run = ExecutionRun::new(&document)?;
            let state = Rc::new(RefCell::new(state));
            let scratch = admission::Scratch::new()?;
            let fake = coordination::dispatcher(&scratch, state.clone());
            let calls = fake.calls.clone();
            let host = LinuxLiveHost::new(
                probe(state),
                LaterUnsupported(case),
                admission::authorization(&run)?,
            );
            let mut coordinator =
                ExecutionCoordinator::new(ExecutionRoot::open(&scratch.0)?, host, fake, &run);
            let result = coordinator.execute_run(&mut run);
            assert!(matches!(result, Err(CoordinatorError::PlanUnavailable(_))));
            assert_eq!(calls.get(), 0);
            assert_eq!(run.state(ActionId::new(0))?, ActionRunState::Waiting);
            assert!(!scratch.0.join("active-attempt.json").exists());
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod final_preservation_default {
    use super::*;
    use storage_engine::{LiveCheckStatus, LiveObservation};
    use storage_runtime::{
        bindings::OutputBindings,
        capability::{BackendCapabilityCheck, OperationAvailability},
        host::{LinuxLiveHost, RuntimeHost},
        lease::ExecutionRoot,
    };

    /// Coverage alone cannot attest protected data contents or preservation throughout execution.
    struct CoverageOnly;
    impl BackendCapabilityCheck for CoverageOnly {
        /// This fixture performs no writes.
        fn availability(
            &self,
            _action: &storage_provider::ProviderAction,
            _observed: &LiveObservation,
        ) -> OperationAvailability {
            OperationAvailability::Unavailable("read-only fixture".into())
        }
        /// Explicit synthetic coverage leaves the independent preservation default untouched.
        fn check_final_scope(
            &self,
            _context: &storage_engine::ExecutionContext<'_>,
            _observed: &LiveObservation,
        ) -> LiveCheckStatus {
            LiveCheckStatus::Satisfied
        }
    }

    /// An unchanged graph cannot stand in for missing evidence about protected contents.
    #[test]
    fn nonempty_protection_requires_independent_preservation_evidence() -> Result<(), Box<dyn Error>>
    {
        let _process = isolate_processes();
        let (state, _) = fixture()?;
        let session = PlanSession::new(HistoricalBaseline::from_lvm_observations(
            state.current.clone(),
            &state.lvm_vgs,
        )?)?;
        let frozen = session.freeze(session.revision())?;
        let mut document = frozen.document().clone();
        document.protected_scope.nodes.push(state.lvm_vgs[0].node);
        let mut run = ExecutionRun::new(&document)?;
        let mut host = LinuxLiveHost::new(
            probe(Rc::new(RefCell::new(state))),
            CoverageOnly,
            admission::authorization(&run)?,
        );
        let observed = host.probe()?;
        let scratch = admission::Scratch::new()?;
        let root = ExecutionRoot::open(&scratch.0)?;
        let lock = root.try_lock_host()?;
        let report = host.verify_goal(&run, &observed, &OutputBindings::new(&run), &lock)?;
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.status == LiveCheckStatus::Satisfied)
        );
        assert!(matches!(
            report.preservation,
            LiveCheckStatus::Unknown { .. }
        ));
        assert!(!run.record_goal_verification(report)?);
        assert!(!run.is_verified());
        Ok(())
    }
}

/// Unrelated existing mounts use durable filesystem UUIDs, not incidental NodeId equality.
#[test]
fn mount_scope_correlates_unrelated_sources_and_rejects_replacements() -> Result<(), Box<dyn Error>>
{
    let _process = isolate_processes();
    use storage_core::model::{FilesystemKind, MountContext, MountSource, ObservedMount};
    let (mut state, _) = fixture()?;
    let vg = state.lvm_vgs[0].node;
    let fs = NodeId::new();
    let mut node = state.current.graph.node(&vg).ok_or("VG")?.clone();
    node.kind.kind = NodeKind::Filesystem {
        kind: FilesystemKind::Ext4,
        label: Some("existing".into()),
    };
    node.size.identities = vec![ExternalId::Filesystem {
        fs_type: "ext4".into(),
        value: "existing-fs".into(),
    }];
    state.current.graph.insert_new(fs, node.clone())?;
    state.current.mounts.entries.push(ObservedMount {
        source: MountSource::Filesystem(fs),
        target: "/existing".into(),
        options: vec!["rw".into(), "nodev".into()],
        context: MountContext::Host,
    });
    let mut session = PlanSession::new(HistoricalBaseline::from_lvm_observations(
        state.current.clone(),
        &state.lvm_vgs,
    )?)?;
    session.add_lv(session.revision(), vg, "data", Bytes::new(16 * 1024 * 1024))?;
    let frozen = session.freeze(session.revision())?;
    let run = ExecutionRun::new(frozen.document())?;
    for case in 0..6 {
        let mut changed = state.clone();
        match case {
            0 => {
                changed = serde_json::from_str(&serde_json::to_string(&changed)?.replace(
                    &fs.as_uuid().to_string(),
                    &NodeId::new().as_uuid().to_string(),
                ))?;
                changed.current.mounts.entries[0].options.reverse();
            }
            1 => {
                let mut replacement = node.clone();
                replacement.size.identities = vec![ExternalId::Filesystem {
                    fs_type: "ext4".into(),
                    value: "replacement".into(),
                }];
                changed.current.graph.replace_existing(fs, replacement)?;
            }
            2 => {
                changed
                    .current
                    .graph
                    .insert_new(NodeId::new(), node.clone())?;
            }
            3 => {
                let mut overmount = changed.current.mounts.entries[0].clone();
                overmount.source = MountSource::Tmpfs;
                changed.current.mounts.entries.push(overmount);
            }
            4 => changed.current.mounts.entries[0].target = "/elsewhere".into(),
            _ => changed.current.mounts.entries[0].options[0] = "ro".into(),
        }
        let observed = ProbedObservation::collect(&probe(Rc::new(RefCell::new(changed))))?;
        assert_eq!(
            resolve_action(&run, ActionId::new(0), observed.observation(), backend()).is_ok(),
            case == 0,
            "case {case}"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[path = "support/ext4.rs"]
mod ext4;

/// Incident methods cannot leave an older regular probe admissible after a separate inspection boundary.
#[test]
fn host_incident_queries_invalidate_prior_admission_publications() -> Result<(), Box<dyn Error>> {
    let _process = isolate_processes();
    use storage_runtime::{
        host::{HostError, RuntimeHost},
        lease::ExecutionRoot,
    };
    let (state, frozen) = fixture()?;
    let state = Rc::new(RefCell::new(state));
    let scratch = admission::Scratch::new()?;
    let run = ExecutionRun::new(frozen.document())?;
    let mut host = coordination::host(state.clone(), &run)?;
    let prior = host.probe()?;
    let root = ExecutionRoot::open(&scratch.0)?;
    assert!(host.inspect_incident(&root)?.is_none());
    let lock = root.try_lock_host()?;
    let dispatcher = coordination::dispatcher(&scratch, state);
    assert!(matches!(
        host.check_plan(&run, &prior, &dispatcher, &lock),
        Err(HostError::ObservationMismatch)
    ));
    Ok(())
}

#[cfg(target_os = "linux")]
#[path = "support/coordinator_crash.rs"]
mod coordinator_crash;
