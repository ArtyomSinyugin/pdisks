//! Direct provider-pass tests use mutable recorded observations, never real device mutations.

use std::{
    cell::{Cell, RefCell},
    error::Error,
};
use storage_core::model::{Bytes, NodeId, NodeKind};
use storage_engine::{HistoricalBaseline, LiveCheckStatus};
use storage_probe::{LinuxProbe, ProbeSnapshot, ProviderProbeError, ProviderState, StateProvider};
use storage_provider::{
    ActionPrerequisite, AllocationDomain, AllocationQuantity, ProviderConnectionKind,
};
use storage_runtime::probe::ProbedObservation;

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
        "../../storage-engine/examples/fixtures/probe_snapshot.json"
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

/// Incomplete applicability cannot admit a real publication; a fresh capacity veto stops the run.
#[test]
fn preflight_retains_observations_without_admitting_dispatch() -> Result<(), Box<dyn Error>> {
    use storage_engine::{
        ActionId, ActionRunState, AdmissionReport, ExecutionRun, editing::PlanSession,
    };
    let source = fixture()?;
    let vg = source.lvm_vgs[0].node;
    let baseline =
        HistoricalBaseline::from_lvm_observations(source.current.clone(), &source.lvm_vgs)?;
    let mut session = PlanSession::new(baseline)?;
    session.add_lv(session.revision(), vg, "data", Bytes::new(16 * 1024 * 1024))?;
    let frozen = session.freeze(session.revision())?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let action = ActionId::new(0);
    run.begin_check(action)?;
    let state = RefCell::new(source);
    let calls = Cell::new(0);
    let probe = LinuxProbe::from_providers(vec![Box::new(Recorded {
        state: &state,
        calls: &calls,
    })]);
    for (epoch, expected) in [(1, ActionRunState::Checking), (2, ActionRunState::Failed)] {
        let publication = ProbedObservation::collect(&probe)?;
        let context = run.context();
        let contract = &context.plan.actions()[0].informational_contract;
        let checks = publication
            .check_prerequisites(context.historical_baseline, &contract.prerequisites)?
            .checks;
        let report = AdmissionReport {
            run_id: run.run_id(),
            action,
            observation: publication.into_observation(),
            applicability: LiveCheckStatus::Unknown {
                required_evidence: "native capability, scope and authorization".into(),
            },
            held_resources: contract.affected_resources.clone(),
            checks,
        };
        assert!(!run.record_admission(report)?);
        assert_eq!(run.state(action)?, expected);
        assert_eq!(
            run.latest_observation()
                .ok_or("observation")?
                .reference
                .epoch
                .get(),
            epoch
        );
        state.borrow_mut().lvm_vgs[0].vg.free = 0;
    }
    assert_eq!(calls.get(), 2);
    Ok(())
}
