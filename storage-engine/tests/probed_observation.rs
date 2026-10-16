//! Direct provider-pass tests use mutable recorded observations, never real device mutations.

use std::{
    cell::{Cell, RefCell},
    error::Error,
};
use storage_core::model::{Bytes, NodeId, NodeKind};
use storage_engine::{HistoricalBaseline, LiveCheckStatus, live::ProbedObservation};
use storage_probe::{LinuxProbe, ProbeSnapshot, ProviderProbeError, ProviderState, StateProvider};
use storage_provider::{
    ActionPrerequisite, AllocationDomain, AllocationQuantity, ProviderConnectionKind,
};

/// Borrows mutable fixture measurements to prove each collection performs exactly one new pass.
struct Recorded<'a> {
    /// Current synthetic provider measurements.
    state: &'a RefCell<ProbeSnapshot>,
    /// Number of actual adapter calls, independent of snapshot generation.
    calls: &'a Cell<u64>,
}
impl StateProvider for Recorded<'_> {
    /// Explicit fixture adapter identity.
    fn id(&self) -> &str {
        "recorded-live-checks"
    }
    /// Exclusive responsibility accepted by the production assembler.
    fn responsibility(&self) -> &str {
        "recorded.storage"
    }
    /// Uses the same built-in orchestration boundary as native adapters.
    fn connection_kind(&self) -> ProviderConnectionKind {
        ProviderConnectionKind::BuiltIn
    }
    /// Returns graph and raw allocation data from one borrowed measurement.
    fn probe(&self) -> Result<ProviderState, ProviderProbeError> {
        self.calls.set(self.calls.get() + 1);
        let value = self.state.borrow();
        Ok(ProviderState {
            graph: value.current.graph.clone(),
            mounts: value.current.mounts.entries.clone(),
            lvm_vgs: value.lvm_vgs.clone(),
            environment: Some(value.current.environment.clone()),
            diagnostics: value.current.diagnostics.clone(),
        })
    }
}

/// Reads one synthetic present VG with exact free-space measurements.
fn fixture() -> Result<ProbeSnapshot, Box<dyn Error>> {
    Ok(serde_json::from_str(include_str!(
        "../examples/fixtures/probe_snapshot.json"
    ))?)
}

/// Constructs the exact frozen capacity condition in the historical identity space.
fn requirement(vg: NodeId) -> ActionPrerequisite {
    ActionPrerequisite::CapacityAvailable {
        domain: AllocationDomain::LvmVolumeGroup(vg),
        amount: Some(AllocationQuantity::new(16)),
    }
}

/// Changed free space is read again; retained publications and historical evidence stay immutable.
#[test]
fn same_pass_capacity_uses_probe_identity_and_current_measurements() -> Result<(), Box<dyn Error>> {
    let source = fixture()?;
    let baseline =
        HistoricalBaseline::from_lvm_observations(source.current.clone(), &source.lvm_vgs)?;
    let old = source.lvm_vgs[0].node;
    let new = NodeId::new();
    let changed: ProbeSnapshot = serde_json::from_str(
        &serde_json::to_string(&source)?
            .replace(&old.as_uuid().to_string(), &new.as_uuid().to_string()),
    )?;
    let state = RefCell::new(changed);
    let calls = Cell::new(0);
    let probe = LinuxProbe::from_providers(vec![Box::new(Recorded {
        state: &state,
        calls: &calls,
    })]);
    let first = ProbedObservation::collect(&probe)?;
    assert_eq!(calls.get(), 1);
    assert_eq!(
        first.observation().reference.probe_instance,
        probe.instance_id()
    );
    assert_eq!(first.observation().reference.epoch.get(), 1);
    let condition = requirement(old);
    assert_eq!(
        first
            .check_prerequisites(&baseline, std::slice::from_ref(&condition))?
            .checks[0]
            .status,
        LiveCheckStatus::Satisfied
    );
    state.borrow_mut().lvm_vgs[0].vg.free = 0;
    let next = ProbedObservation::collect(&probe)?;
    assert_eq!(calls.get(), 2);
    assert_eq!(next.observation().reference.epoch.get(), 2);
    assert!(matches!(
        next.check_prerequisites(&baseline, std::slice::from_ref(&condition))?
            .checks[0]
            .status,
        LiveCheckStatus::Violated { .. }
    ));
    assert_eq!(
        first
            .check_prerequisites(&baseline, std::slice::from_ref(&condition))?
            .checks[0]
            .status,
        LiveCheckStatus::Satisfied
    );
    assert_eq!(calls.get(), 2);
    let other = LinuxProbe::from_providers(vec![Box::new(Recorded {
        state: &state,
        calls: &calls,
    })]);
    let restarted = ProbedObservation::collect(&other)?;
    assert_eq!(restarted.observation().reference.epoch.get(), 1);
    assert_ne!(
        restarted.observation().reference.probe_instance,
        first.observation().reference.probe_instance
    );
    assert_eq!(first.into_observation().reference.epoch.get(), 1);
    Ok(())
}

/// Invalid raw records and changed allocation units cannot reuse old historical free counts.
#[test]
fn missing_ambiguous_and_changed_geometry_measurements_remain_unknown() -> Result<(), Box<dyn Error>>
{
    let source = fixture()?;
    let vg = source.lvm_vgs[0].node;
    let baseline =
        HistoricalBaseline::from_lvm_observations(source.current.clone(), &source.lvm_vgs)?;
    for case in 0..7 {
        let mut value = source.clone();
        match case {
            0 => value.lvm_vgs.clear(),
            1 => value.lvm_vgs.push(value.lvm_vgs[0].clone()),
            2 => value.lvm_vgs[0].vg.uuid = "wrong-uuid".into(),
            3 => value.lvm_vgs[0].vg.free = 1,
            4 => value.lvm_vgs[0].vg.exported = true,
            5 => {
                value.lvm_vgs[0].vg.extent_size *= 2;
                let mut node = value.current.graph.node(&vg).ok_or("VG")?.clone();
                let NodeKind::LvmVg { extent_size, .. } = &mut node.kind.kind else {
                    return Err("VG kind".into());
                };
                *extent_size = Some(Bytes::new(value.lvm_vgs[0].vg.extent_size));
                value.current.graph.replace_existing(vg, node)?;
            }
            _ => value
                .current
                .diagnostics
                .push(storage_core::model::Diagnostic {
                    code: "provider.failure".into(),
                    severity: storage_core::model::DiagnosticSeverity::Error,
                    subjects: Vec::new(),
                    message: "incomplete measurement".into(),
                    evidence: None,
                    suggested_remedy: None,
                }),
        }
        let state = RefCell::new(value);
        let calls = Cell::new(0);
        let probe = LinuxProbe::from_providers(vec![Box::new(Recorded {
            state: &state,
            calls: &calls,
        })]);
        let observed = ProbedObservation::collect(&probe)?;
        let report = observed.check_prerequisites(&baseline, &[requirement(vg)])?;
        assert!(
            matches!(report.checks[0].status, LiveCheckStatus::Unknown { .. }),
            "case {case}"
        );
        let unknown_amount = ActionPrerequisite::CapacityAvailable {
            domain: AllocationDomain::LvmVolumeGroup(vg),
            amount: None,
        };
        assert!(matches!(
            observed
                .check_prerequisites(&baseline, &[unknown_amount])?
                .checks[0]
                .status,
            LiveCheckStatus::Unknown { .. }
        ));
        assert_eq!(calls.get(), 1);
    }
    Ok(())
}

/// A leased preflight really probes, rejects probe substitution, and never manufactures admission.
#[cfg(target_os = "linux")]
#[test]
fn leased_preflight_publishes_fresh_checks_without_enabling_dispatch() -> Result<(), Box<dyn Error>>
{
    use std::{fs, os::unix::fs::DirBuilderExt, path::PathBuf};
    use storage_engine::{
        ActionId, ActionRunState, ExecutionRun, ExecutionRunError,
        editing::PlanSession,
        journal::{DurableJournal, JournalLog, RecoveryStatus},
        leases::{ExecutionLease, LeaseError},
    };
    /// Cleans only this test's private scratch root after its guards and journals have dropped.
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        /// Production code retains the persistent lock inode; test cleanup follows guard teardown.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let root = Scratch(std::env::temp_dir().join(format!(
        "disksgraph-preflight-test-{}",
        uuid::Uuid::now_v7()
    )));
    fs::DirBuilder::new().mode(0o700).create(&root.0)?;
    let source = fixture()?;
    let vg = source.lvm_vgs[0].node;
    let baseline =
        HistoricalBaseline::from_lvm_observations(source.current.clone(), &source.lvm_vgs)?;
    let mut session = PlanSession::new(baseline)?;
    session.add_lv(session.revision(), vg, "data", Bytes::new(16 * 1024 * 1024))?;
    let frozen = session.freeze(session.revision())?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let mut other = ExecutionRun::new(frozen.document())?;
    let path = root.0.join("run.jsonl");
    let mut journal = DurableJournal::create(&path, &frozen, &run)?;
    let action = ActionId::new(0);
    run.begin_check(action)?;
    other.begin_check(action)?;
    let guard = ExecutionLease::try_acquire(&root.0, &run, action)?;
    let state = RefCell::new(source.clone());
    let calls = Cell::new(0);
    let probe = LinuxProbe::from_providers(vec![Box::new(Recorded {
        state: &state,
        calls: &calls,
    })]);
    assert!(matches!(
        guard.record_probe_preflight(&mut other, &probe),
        Err(LeaseError::RunMismatch)
    ));
    assert_eq!(calls.get(), 0);
    guard.record_probe_preflight(&mut run, &probe)?;
    assert_eq!(calls.get(), 1);
    assert_eq!(run.state(action)?, ActionRunState::Checking);
    let admission = run.admission(action)?.ok_or("preflight")?;
    assert!(matches!(
        admission.applicability,
        LiveCheckStatus::Unknown { .. }
    ));
    assert!(
        admission
            .checks
            .iter()
            .all(|check| check.status == LiveCheckStatus::Satisfied)
    );
    assert_eq!(admission.held_resources, guard.held_resources());
    assert_eq!(
        run.latest_observation()
            .ok_or("actual snapshot")?
            .snapshot
            .graph
            .nodes()
            .len(),
        1
    );
    let retained = run.latest_observation().cloned();
    let events = run.events_since(0).count();
    let other_probe = LinuxProbe::from_providers(vec![Box::new(Recorded {
        state: &state,
        calls: &calls,
    })]);
    assert!(matches!(
        guard.record_probe_preflight(&mut run, &other_probe),
        Err(LeaseError::Run(ExecutionRunError::StaleObservation))
    ));
    assert_eq!(calls.get(), 1);
    assert_eq!(run.latest_observation(), retained.as_ref());
    assert_eq!(run.events_since(0).count(), events);
    state
        .borrow_mut()
        .current
        .graph
        .insert_dependency(storage_core::model::Dependency {
            from: NodeId::new(),
            to: NodeId::new(),
            kind: storage_core::model::DependencyKind::Backs,
        });
    assert!(guard.record_probe_preflight(&mut run, &probe).is_err());
    assert_eq!(run.latest_observation(), retained.as_ref());
    assert_eq!(run.events_since(0).count(), events);
    *state.borrow_mut() = source;
    state.borrow_mut().lvm_vgs[0].vg.free = 0;
    guard.record_probe_preflight(&mut run, &probe)?;
    assert_eq!(calls.get(), 3);
    assert_eq!(run.state(action)?, ActionRunState::Failed);
    assert_eq!(
        run.latest_observation()
            .ok_or("fresh snapshot")?
            .reference
            .epoch
            .get(),
        3
    );
    assert!(
        run.admission(action)?
            .ok_or("failed preflight")?
            .checks
            .iter()
            .any(|check| matches!(check.status, LiveCheckStatus::Violated { .. }))
    );
    assert!(guard.record_dispatch_intent(&mut journal, &run).is_err());
    journal.flush_events(&run)?;
    assert_eq!(
        JournalLog::read(&path)?.review()?.actions[0].status,
        RecoveryStatus::NoInvocationRecorded
    );
    assert!(matches!(
        ExecutionLease::try_acquire(&root.0, &other, action),
        Err(LeaseError::Busy)
    ));
    assert!(guard.record_probe_preflight(&mut run, &probe).is_err());
    assert_eq!(calls.get(), 3);
    drop(guard);
    Ok(())
}
