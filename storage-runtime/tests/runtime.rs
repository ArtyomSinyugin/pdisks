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
