//! Recorded C4 runtime fixtures; all storage mutations happen only in Rust memory.

use super::*;
use storage_core::model::FilesystemKind;
use storage_engine::ActionRunState;
use storage_runtime::{
    coordinator::ExecutionCoordinator, lease::ExecutionRoot, targets::resolve_action_with_bindings,
};

/// Builds the roadmap's LV creation, activation and ext4 prefix using normal plan editing.
fn filesystem_plan() -> Result<(ProbeSnapshot, FrozenPlan, NodeId), Box<dyn Error>> {
    let (state, _) = fixture()?;
    let baseline =
        HistoricalBaseline::from_lvm_observations(state.current.clone(), &state.lvm_vgs)?;
    let mut session = PlanSession::new(baseline)?;
    let lv = session
        .add_lv(
            session.revision(),
            state.lvm_vgs[0].node,
            "data",
            Bytes::new(16 * 1024 * 1024),
        )?
        .created_node
        .ok_or("planned LV")?;
    session.add_filesystem(session.revision(), lv, FilesystemKind::Ext4)?;
    Ok((state, session.freeze(session.revision())?, lv))
}

/// Ext4 resolution requires the same run's verified LV plus fresh active writable endpoint evidence.
#[test]
fn ext4_resolution_requires_verified_active_unused_lv() -> Result<(), Box<dyn Error>> {
    let (state, frozen, _) = filesystem_plan()?;
    let state = Rc::new(RefCell::new(state));
    let scratch = admission::Scratch::new()?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let mut fake = coordination::dispatcher(&scratch, state.clone());
    fake.created_uuid = Some("created-lv".into());
    let mut coordinator = ExecutionCoordinator::new(
        ExecutionRoot::open(&scratch.0)?,
        coordination::host(state, &run)?,
        fake,
        &run,
    );
    assert_eq!(
        coordinator.execute_action(&mut run, ActionId::new(0), None)?,
        ActionRunState::Verified
    );
    let inactive = run.latest_observation().ok_or("completion")?;
    assert!(
        resolve_action_with_bindings(
            &run,
            ActionId::new(2),
            inactive,
            backend(),
            coordinator.bindings()
        )
        .is_err()
    );
    assert_eq!(
        coordinator.execute_action(&mut run, ActionId::new(1), None)?,
        ActionRunState::Verified
    );
    let observed = run.latest_observation().ok_or("completion")?;
    let resolved = resolve_action_with_bindings(
        &run,
        ActionId::new(2),
        observed,
        backend(),
        coordinator.bindings(),
    )?;
    assert!(resolved.lv_target().ok_or("LV")?.block.is_some());
    for case in 0..4 {
        let mut changed = observed.clone();
        let id = resolved.lv_target().ok_or("LV")?.observed_id;
        let mut node = changed.snapshot.graph.node(&id).ok_or("node")?.clone();
        match case {
            0 => node.size.lvm_active = None,
            1 => node.size.block = None,
            2 => {
                if let Some(block) = &mut node.size.block {
                    block.read_only = Some(true);
                }
            }
            _ => {
                if let Some(block) = &mut node.size.block {
                    block.paths = vec!["relative".into()];
                }
            }
        }
        changed.snapshot.graph.replace_existing(id, node)?;
        assert!(
            resolve_action_with_bindings(
                &run,
                ActionId::new(2),
                &changed,
                backend(),
                coordinator.bindings()
            )
            .is_err(),
            "case {case}"
        );
    }
    assert!(
        resolve_action_with_bindings(
            &run,
            ActionId::new(2),
            observed,
            backend(),
            &storage_runtime::bindings::OutputBindings::new(&run)
        )
        .is_err()
    );
    Ok(())
}

/// Explicit consumer coverage for this private in-memory graph only; no host disks are inspected.
struct FilesystemPolicy;
impl storage_runtime::capability::BackendCapabilityCheck for FilesystemPolicy {
    /// Every operation in this fixture is implemented by its synthetic dispatcher.
    fn availability(
        &self,
        _: &storage_provider::ProviderAction,
        _: &storage_engine::LiveObservation,
    ) -> storage_runtime::capability::OperationAvailability {
        storage_runtime::capability::OperationAvailability::Available(backend())
    }
    /// The fixture owns the entire synthetic topology and no external writers exist.
    fn check_scope(
        &self,
        _: &storage_engine::ExecutionContext<'_>,
        _: &storage_runtime::targets::ResolvedAction,
        _: &storage_engine::LiveObservation,
    ) -> storage_engine::LiveCheckStatus {
        storage_engine::LiveCheckStatus::Satisfied
    }
    /// All consumers of these in-memory objects are contained in the test's graph.
    fn check_consumer_exclusion(
        &self,
        _: &storage_engine::ExecutionContext<'_>,
        _: &storage_runtime::targets::ResolvedAction,
        _: &storage_engine::LiveObservation,
    ) -> storage_engine::LiveCheckStatus {
        storage_engine::LiveCheckStatus::Satisfied
    }
    /// Explicit complete final coverage applies only to these synthetic publications.
    fn check_final_scope(
        &self,
        _: &storage_engine::ExecutionContext<'_>,
        _: &storage_engine::LiveObservation,
    ) -> storage_engine::LiveCheckStatus {
        storage_engine::LiveCheckStatus::Satisfied
    }
}

/// General authorization leaves destructive confirmation Unknown even with complete consumer evidence.
#[test]
fn ext4_admission_requires_explicit_consumer_exclusion_and_destructive_confirmation()
-> Result<(), Box<dyn Error>> {
    use storage_runtime::host::{LinuxLiveHost, RuntimeHost};
    /// Exercises the same coordinator path for hosts with independent policy types.
    fn check<H: RuntimeHost>(
        scratch: &admission::Scratch,
        state: Rc<RefCell<ProbeSnapshot>>,
        mut run: ExecutionRun,
        host: H,
    ) -> Result<(), Box<dyn Error>> {
        let mut fake = coordination::dispatcher(scratch, state);
        fake.created_uuid = Some("created-lv".into());
        let calls = fake.calls.clone();
        let mut coordinator =
            ExecutionCoordinator::new(ExecutionRoot::open(&scratch.0)?, host, fake, &run);
        for index in 0..2 {
            assert_eq!(
                coordinator.execute_action(&mut run, ActionId::new(index), None)?,
                ActionRunState::Verified
            );
        }
        assert_eq!(
            coordinator.execute_action(&mut run, ActionId::new(2), None)?,
            ActionRunState::Checking
        );
        assert_eq!(calls.get(), 2);
        assert!(!scratch.0.join("active-attempt.json").exists());
        assert_eq!(coordinator.bindings().len(), 1);
        Ok(())
    }
    let (state, frozen, _) = filesystem_plan()?;
    let state = Rc::new(RefCell::new(state));
    let scratch = admission::Scratch::new()?;
    let run = ExecutionRun::new(frozen.document())?;
    let host = coordination::host(state.clone(), &run)?;
    check(&scratch, state, run, host)?;

    let (state, frozen, _) = filesystem_plan()?;
    let state = Rc::new(RefCell::new(state));
    let scratch = admission::Scratch::new()?;
    let run = ExecutionRun::new(frozen.document())?;
    let host = LinuxLiveHost::new(
        probe(state.clone()),
        FilesystemPolicy,
        admission::authorization(&run)?,
    );
    check(&scratch, state, run, host)?;
    Ok(())
}

/// Explicit destructive approval for the sole verified LV of this synthetic plan.
struct FilesystemApproval {
    /// Existing exact digest/run/backend policy used for ordinary execution authorization.
    inner: admission::Authorization,
    /// Planned LV whose content replacement the fixture explicitly approves.
    backing: NodeId,
    /// Independent explicit destructive confirmation, separate from ordinary execution approval.
    confirmation: storage_engine::LiveCheckStatus,
}
impl storage_runtime::authorization::AuthorizationContext for FilesystemApproval {
    /// Retains all normal principal, digest, target and observation checks.
    fn check(
        &mut self,
        request: &storage_runtime::authorization::ApprovalRequest,
    ) -> storage_engine::LiveCheckStatus {
        storage_runtime::authorization::AuthorizationContext::check(&mut self.inner, request)
    }
    /// Approval is separate and limited to a fingerprinted LV, never a display path.
    fn check_destructive(
        &mut self,
        request: &storage_runtime::authorization::ApprovalRequest,
        target: NodeId,
    ) -> storage_engine::LiveCheckStatus {
        if target == self.backing
            && request.targets.iter().any(|item| {
                item.planned_id == target
                    && matches!(item.external_ids.as_slice(), [ExternalId::LvmLvUuid(_)])
            })
        {
            let principal = storage_runtime::authorization::AuthorizationContext::check(
                &mut self.inner,
                request,
            );
            if principal == storage_engine::LiveCheckStatus::Satisfied {
                self.confirmation.clone()
            } else {
                principal
            }
        } else {
            storage_engine::LiveCheckStatus::Violated {
                reason: "wrong destructive target".into(),
            }
        }
    }
}

/// Post-dispatch failure modes injected only into private recorded storage state.
#[derive(Clone, Copy, Debug)]
enum FilesystemFault {
    /// Successful independent UUID and ownership observations.
    None,
    /// Backend success lacks filesystem UUID provenance.
    MissingReceipt,
    /// Backend reports a different UUID from the actual observed result.
    WrongReceipt,
    /// Backend supplies an LV identity for a filesystem action.
    WrongReceiptKind,
    /// Two filesystem nodes advertise the same new UUID.
    DuplicateUuid,
    /// A different backing owns the observed filesystem.
    WrongBacking,
    /// Actual label differs from the requested create properties.
    WrongLabel,
    /// Invocation fails after publishing an effect, leaving partial completion uncertain.
    AdapterError,
    /// Another filesystem consumer appeared alongside the requested result.
    ExtraConsumer,
    /// A partial provider publication cannot establish positive postconditions.
    BlockingDiagnostics,
}

/// Synthetic ext4 adapter layered over the existing private-memory LV dispatcher.
struct FilesystemDispatcher {
    /// LV creation and activation retain their existing checkpoint assertions and independent observations.
    lvm: coordination::FakeDispatcher,
    /// Canonical new UUID returned only after this fixture publishes the ext4 result.
    uuid: String,
    /// Explicit result corruption used to verify checkpoint retention and stopped execution.
    fault: FilesystemFault,
}
impl storage_runtime::dispatcher::BackendDispatcher for FilesystemDispatcher {
    /// One composite test backend is fixed throughout the plan; no dispatch-time fallback exists.
    fn backend_id(&self) -> &BackendId {
        &self.lvm.backend
    }
    /// Changes private recorded state only after checking the durable checkpoint and held OS lock.
    fn execute(
        &mut self,
        action: &storage_runtime::targets::ResolvedAction,
        progress: &mut dyn FnMut(String) -> Result<(), storage_engine::ExecutionRunError>,
    ) -> Result<
        storage_runtime::dispatcher::ExecutionReceipt,
        storage_runtime::dispatcher::DispatchError,
    > {
        use storage_core::model::{
            Dependency, DependencyKind, Node, NodeFacts, NodeSpec, Presence,
        };
        use storage_runtime::{
            checkpoint::ActiveAttempt,
            dispatcher::{DispatchError, ExecutionReceipt},
        };
        let storage_provider::ProviderAction::Filesystem(
            storage_provider::FilesystemAction::Create { label, .. },
        ) = action.operation()
        else {
            return storage_runtime::dispatcher::BackendDispatcher::execute(
                &mut self.lvm,
                action,
                progress,
            );
        };
        let attempt = std::fs::read(self.lvm.root.join("active-attempt.json"))
            .map_err(|e| DispatchError::Adapter(e.to_string()))?;
        let attempt = ActiveAttempt::from_bytes(&attempt)
            .map_err(|e| DispatchError::Adapter(e.to_string()))?;
        assert_eq!(attempt.action, *action.operation());
        assert_eq!(attempt.targets, action.fingerprints());
        assert!(
            ExecutionRoot::open(&self.lvm.root)
                .unwrap_or_else(|e| panic!("root: {e}"))
                .try_lock_host()
                .is_err()
        );
        self.lvm.calls.set(self.lvm.calls.get() + 1);
        let backing = action
            .lv_target()
            .ok_or_else(|| DispatchError::Adapter("missing backing".into()))?;
        assert!(backing.block.is_some());
        let mut state = self.lvm.state.borrow_mut();
        let id = NodeId::new();
        state
            .current
            .graph
            .insert_new(
                id,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Filesystem {
                            kind: FilesystemKind::Ext4,
                            label: label.clone(),
                        },
                        size: Some(Bytes::new(15 * 1024 * 1024)),
                    },
                    size: NodeFacts {
                        presence: Presence::Present,
                        identities: vec![ExternalId::Filesystem {
                            fs_type: "ext4".into(),
                            value: self.uuid.clone(),
                        }],
                        ..NodeFacts::default()
                    },
                },
            )
            .map_err(|e| DispatchError::Adapter(e.to_string()))?;
        state.current.graph.insert_dependency(Dependency {
            from: backing.observed_id,
            to: id,
            kind: DependencyKind::Backs,
        });
        let mut receipt = ExecutionReceipt {
            outcome: storage_engine::BackendOutcome::Succeeded,
            created_lv_uuid: None,
            created_filesystem_uuid: Some(self.uuid.clone()),
        };
        match self.fault {
            FilesystemFault::None => {}
            FilesystemFault::MissingReceipt => receipt.created_filesystem_uuid = None,
            FilesystemFault::WrongReceipt => {
                receipt.created_filesystem_uuid = Some(uuid::Uuid::now_v7().to_string())
            }
            FilesystemFault::WrongReceiptKind => {
                receipt.created_lv_uuid = Some("wrong-kind".into());
                receipt.created_filesystem_uuid = None;
            }
            FilesystemFault::DuplicateUuid | FilesystemFault::ExtraConsumer => {
                let mut node = state
                    .current
                    .graph
                    .node(&id)
                    .cloned()
                    .ok_or_else(|| DispatchError::Adapter("FS".into()))?;
                if matches!(self.fault, FilesystemFault::ExtraConsumer) {
                    node.size.identities = vec![ExternalId::Filesystem {
                        fs_type: "ext4".into(),
                        value: uuid::Uuid::now_v7().to_string(),
                    }];
                }
                let extra = NodeId::new();
                state
                    .current
                    .graph
                    .insert_new(extra, node)
                    .map_err(|e| DispatchError::Adapter(e.to_string()))?;
                state.current.graph.insert_dependency(Dependency {
                    from: backing.observed_id,
                    to: extra,
                    kind: DependencyKind::Backs,
                });
            }
            FilesystemFault::WrongBacking => {
                state.current.graph.remove_dependency(&Dependency {
                    from: backing.observed_id,
                    to: id,
                    kind: DependencyKind::Backs,
                });
                state.current.graph.insert_dependency(Dependency {
                    from: action.target().observed_id,
                    to: id,
                    kind: DependencyKind::Backs,
                });
            }
            FilesystemFault::WrongLabel => {
                let mut node = state
                    .current
                    .graph
                    .node(&id)
                    .cloned()
                    .ok_or_else(|| DispatchError::Adapter("FS".into()))?;
                node.kind.kind = NodeKind::Filesystem {
                    kind: FilesystemKind::Ext4,
                    label: Some("unrequested".into()),
                };
                state
                    .current
                    .graph
                    .replace_existing(id, node)
                    .map_err(|e| DispatchError::Adapter(e.to_string()))?;
            }
            FilesystemFault::AdapterError => {
                return Err(DispatchError::Adapter(
                    "synthetic failure after partial effect".into(),
                ));
            }
            FilesystemFault::BlockingDiagnostics => {
                state
                    .current
                    .diagnostics
                    .push(storage_core::model::Diagnostic {
                        severity: storage_core::model::DiagnosticSeverity::Error,
                        code: "fixture.incomplete".into(),
                        message: "synthetic incomplete filesystem probe".into(),
                        subjects: vec![],
                        evidence: None,
                        suggested_remedy: None,
                    });
            }
        }
        Ok(receipt)
    }
}

/// The roadmap's three-action prefix verifies actual ext4 output and rechecks it after transient ID drift.
#[test]
fn lv_activation_ext4_run_verifies_and_retains_owned_filesystem() -> Result<(), Box<dyn Error>> {
    use storage_runtime::host::LinuxLiveHost;
    let (state, frozen, backing) = filesystem_plan()?;
    let state = Rc::new(RefCell::new(state));
    let scratch = admission::Scratch::new()?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let mut lvm = coordination::dispatcher(&scratch, state.clone());
    lvm.created_uuid = Some("created-lv".into());
    let calls = lvm.calls.clone();
    let host = LinuxLiveHost::new(
        probe(state),
        FilesystemPolicy,
        FilesystemApproval {
            inner: admission::authorization(&run)?,
            backing,
            confirmation: storage_engine::LiveCheckStatus::Satisfied,
        },
    );
    let dispatcher = FilesystemDispatcher {
        lvm,
        uuid: uuid::Uuid::now_v7().to_string(),
        fault: FilesystemFault::None,
    };
    let mut coordinator =
        ExecutionCoordinator::new(ExecutionRoot::open(&scratch.0)?, host, dispatcher, &run);
    let summary = coordinator.execute_run(&mut run)?;
    assert!(summary.verified);
    assert_eq!(calls.get(), 3);
    assert_eq!(coordinator.bindings().len(), 2);
    assert!(!scratch.0.join("active-attempt.json").exists());
    let storage_provider::ProviderAction::Filesystem(storage_provider::FilesystemAction::Create {
        planned_node_id: filesystem,
        ..
    }) = run.context().plan.actions()[2].operation
    else {
        return Err("FS action".into());
    };
    let observed = summary.observation.ok_or("final observation")?;
    let fs = coordinator
        .bindings()
        .resolve(&run, filesystem, &observed)?;
    let lv = coordinator.bindings().resolve(&run, backing, &observed)?;
    let new_fs = NodeId::new();
    let new_lv = NodeId::new();
    let changed = serde_json::to_string(&observed)?
        .replace(&fs.as_uuid().to_string(), &new_fs.as_uuid().to_string())
        .replace(&lv.as_uuid().to_string(), &new_lv.as_uuid().to_string());
    let changed = serde_json::from_str(&changed)?;
    assert_eq!(
        coordinator.bindings().resolve(&run, filesystem, &changed)?,
        new_fs
    );
    for case in 0..3 {
        let mut wrong: storage_engine::LiveObservation = changed.clone();
        let mut node = wrong.snapshot.graph.node(&new_fs).ok_or("FS")?.clone();
        match case {
            0 => node.kind.size = Some(Bytes::new(4096)),
            1 => {
                node.size.identities = vec![ExternalId::Filesystem {
                    fs_type: "ext4".into(),
                    value: uuid::Uuid::now_v7().to_string(),
                }]
            }
            _ => node.size.presence = storage_core::model::Presence::Unknown,
        }
        wrong.snapshot.graph.replace_existing(new_fs, node)?;
        assert!(
            coordinator
                .bindings()
                .resolve(&run, filesystem, &wrong)
                .is_err(),
            "case {case}"
        );
    }
    Ok(())
}

/// Backend success, partial effects and malformed evidence all preserve the marker and forbid retry.
#[test]
fn ext4_failures_keep_checkpoint_and_never_publish_a_binding_or_retry() -> Result<(), Box<dyn Error>>
{
    use storage_runtime::{coordinator::CoordinatorError, host::LinuxLiveHost};
    for fault in [
        FilesystemFault::MissingReceipt,
        FilesystemFault::WrongReceipt,
        FilesystemFault::WrongReceiptKind,
        FilesystemFault::DuplicateUuid,
        FilesystemFault::WrongBacking,
        FilesystemFault::WrongLabel,
        FilesystemFault::AdapterError,
        FilesystemFault::ExtraConsumer,
        FilesystemFault::BlockingDiagnostics,
    ] {
        let (state, frozen, backing) = filesystem_plan()?;
        let state = Rc::new(RefCell::new(state));
        let scratch = admission::Scratch::new()?;
        let mut run = ExecutionRun::new(frozen.document())?;
        let mut lvm = coordination::dispatcher(&scratch, state.clone());
        lvm.created_uuid = Some("created-lv".into());
        let calls = lvm.calls.clone();
        let host = LinuxLiveHost::new(
            probe(state.clone()),
            FilesystemPolicy,
            FilesystemApproval {
                inner: admission::authorization(&run)?,
                backing,
                confirmation: storage_engine::LiveCheckStatus::Satisfied,
            },
        );
        let dispatcher = FilesystemDispatcher {
            lvm,
            uuid: uuid::Uuid::now_v7().to_string(),
            fault,
        };
        let mut coordinator =
            ExecutionCoordinator::new(ExecutionRoot::open(&scratch.0)?, host, dispatcher, &run);
        let result = coordinator.execute_run(&mut run);
        assert!(result.is_err() || !result?.verified, "{fault:?}");
        assert_eq!(
            run.state(ActionId::new(2))?,
            ActionRunState::OutcomeUnknown,
            "{fault:?}"
        );
        assert!(!run.is_verified(), "{fault:?}");
        assert_eq!(calls.get(), 3, "{fault:?}");
        assert_eq!(coordinator.bindings().len(), 1, "{fault:?}");
        let marker = std::fs::read(scratch.0.join("active-attempt.json"))?;
        let attempt = storage_runtime::checkpoint::ActiveAttempt::from_bytes(&marker)?;
        assert_eq!(attempt.action_id, ActionId::new(2));
        assert!(
            matches!(
                coordinator.execute_run(&mut run),
                Err(CoordinatorError::Stopped)
            ),
            "{fault:?}"
        );
        assert_eq!(calls.get(), 3);
        assert_eq!(
            std::fs::read(scratch.0.join("active-attempt.json"))?,
            marker
        );
        let root = ExecutionRoot::open(&scratch.0)?;
        let incident =
            storage_runtime::incident::inspect_incident(&root, &probe(state))?.ok_or("incident")?;
        assert_eq!(incident.attempt.action_id, ActionId::new(2));
        assert_eq!(
            incident.assessment,
            storage_runtime::incident::IncidentAssessment::Indeterminate
        );
        assert_eq!(
            std::fs::read(scratch.0.join("active-attempt.json"))?,
            marker
        );
    }
    Ok(())
}

/// Adding an ext4 action cannot make the native LVM adapter partially execute an unsupported plan.
#[test]
fn native_lvm_rejects_ext4_plan_before_first_write() -> Result<(), Box<dyn Error>> {
    use storage_runtime::{
        coordinator::CoordinatorError, host::LinuxLiveHost, native_lvm::NativeLvmAdapter,
    };
    let library = mock_lvm::Fixture::new(0)?;
    let backend = library.registry.backends().next().ok_or("backend")?;
    let before = backend.probe_lvm()?;
    let adapter = NativeLvmAdapter::new(backend)?;
    let (state, frozen, _) = filesystem_plan()?;
    let scratch = admission::Scratch::new()?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let host = LinuxLiveHost::new(
        probe(Rc::new(RefCell::new(state))),
        adapter,
        admission::authorization(&run)?,
    );
    let mut coordinator =
        ExecutionCoordinator::new(ExecutionRoot::open(&scratch.0)?, host, adapter, &run);
    assert!(matches!(
        coordinator.execute_run(&mut run),
        Err(CoordinatorError::PlanUnavailable(_))
    ));
    assert_eq!(run.state(ActionId::new(0))?, ActionRunState::Waiting);
    let after = backend.probe_lvm()?;
    assert_eq!(before.lvs.len(), after.lvs.len());
    assert_eq!(before.vgs[0].free, after.vgs[0].free);
    assert!(!scratch.0.join("active-attempt.json").exists());
    Ok(())
}

/// An explicit destructive veto stops admission without arming a filesystem checkpoint or calling mkfs.
#[test]
fn ext4_destructive_veto_stops_before_dispatch() -> Result<(), Box<dyn Error>> {
    use storage_runtime::host::LinuxLiveHost;
    let (state, frozen, backing) = filesystem_plan()?;
    let state = Rc::new(RefCell::new(state));
    let scratch = admission::Scratch::new()?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let mut lvm = coordination::dispatcher(&scratch, state.clone());
    lvm.created_uuid = Some("created-lv".into());
    let calls = lvm.calls.clone();
    let host = LinuxLiveHost::new(
        probe(state),
        FilesystemPolicy,
        FilesystemApproval {
            inner: admission::authorization(&run)?,
            backing,
            confirmation: storage_engine::LiveCheckStatus::Violated {
                reason: "content replacement was explicitly denied".into(),
            },
        },
    );
    let dispatcher = FilesystemDispatcher {
        lvm,
        uuid: uuid::Uuid::now_v7().to_string(),
        fault: FilesystemFault::None,
    };
    let mut coordinator =
        ExecutionCoordinator::new(ExecutionRoot::open(&scratch.0)?, host, dispatcher, &run);
    let summary = coordinator.execute_run(&mut run)?;
    assert!(!summary.verified);
    assert_eq!(summary.stopped_action, Some(ActionId::new(2)));
    assert_eq!(run.state(ActionId::new(2))?, ActionRunState::Failed);
    assert_eq!(calls.get(), 2);
    assert!(!scratch.0.join("active-attempt.json").exists());
    assert_eq!(coordinator.bindings().len(), 1);
    Ok(())
}
