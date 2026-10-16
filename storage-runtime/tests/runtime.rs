//! Synthetic runtime recipes exercise real locks and checkpoints, never native storage writes.

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
    let state: ProbeSnapshot = serde_json::from_str(include_str!(
        "../../storage-engine/examples/fixtures/probe_snapshot.json"
    ))?;
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
                || request.targets.len() != 1
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
            let decision = host.check_admission(&run, action, &observed, &lease)?;
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
            host.check_admission(&other, action, &observed, &lease),
            Err(HostError::LeaseMismatch)
        ));
        let foreign = ProbedObservation::collect(&probe(state))?;
        assert!(matches!(
            host.check_admission(&run, action, &foreign, &lease),
            Err(HostError::ObservationMismatch)
        ));
        let latest = host.probe()?;
        assert!(matches!(
            host.check_admission(&run, action, &observed, &lease),
            Err(HostError::ObservationMismatch)
        ));
        let decision = host.check_admission(&run, action, &latest, &lease)?;
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
    /// Receipt UUID plus independent LV properties/ownership is required before clear or binding.
    #[test]
    fn created_lv_binding_requires_verified_result_and_exact_owner() -> Result<(), Box<dyn Error>> {
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
