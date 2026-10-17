//! Final goal checks use synthetic actual publications, never a simulation success flag.

use std::error::Error;
use storage_core::model::{
    BlockFacts, BlockGeometry, BlockSize, Bytes, DeviceNumber, ExternalId, NodeId, ProbeEpoch,
};
use storage_engine::{
    ExecutionRun, HistoricalBaseline, LiveCheckStatus, LiveObservation, ObservationReference,
    editing::{NodeCorrelation, PlanSession},
    live::check_observed_goal,
};
use uuid::Uuid;

/// A baseline with one UUID-qualified VG and recorded same-pass capacity.
fn session() -> Result<PlanSession, Box<dyn Error>> {
    let raw: serde_json::Value =
        serde_json::from_str(include_str!("../examples/fixtures/probe_snapshot.json"))?;
    let baseline = HistoricalBaseline::from_lvm_observations(
        serde_json::from_value(raw["current"].clone())?,
        &serde_json::from_value::<Vec<storage_provider::LvmVgObservation>>(raw["lvm_vgs"].clone())?,
    )?;
    Ok(PlanSession::new(baseline)?)
}

/// A final goal needs actual bindings, activation, writable facts and complete unambiguous topology.
#[test]
fn final_goal_rechecks_outputs_and_activation() -> Result<(), Box<dyn Error>> {
    let mut session = session()?;
    let vg = *session
        .baseline()
        .snapshot
        .graph
        .nodes()
        .next()
        .ok_or("VG")?
        .0;
    session.add_lv(session.revision(), vg, "data", Bytes::new(16 * 1024 * 1024))?;
    let frozen = session.freeze(session.revision())?;
    let run = ExecutionRun::new(frozen.document())?;
    let planned = run
        .context()
        .requested_goal
        .graph
        .nodes()
        .find(|(id, _)| **id != vg)
        .map(|(id, _)| *id)
        .ok_or("LV")?;
    let actual = NodeId::new();
    let mut snapshot = run.context().historical_baseline.snapshot.clone();
    snapshot.graph = serde_json::from_str(
        &serde_json::to_string(&run.context().requested_goal.graph)?.replace(
            &planned.as_uuid().to_string(),
            &actual.as_uuid().to_string(),
        ),
    )?;
    let mut node = snapshot.graph.node(&actual).ok_or("LV")?.clone();
    node.size.identities = vec![ExternalId::LvmLvUuid("new-lv".into())];
    node.size.presence = storage_core::model::Presence::Present;
    node.size.lvm_active = Some(true);
    node.size.block = Some(BlockFacts {
        paths: vec!["/dev/mock-lv".into()],
        devno: DeviceNumber {
            major: 253,
            minor: 1,
        },
        read_only: Some(false),
        geometry: Some(BlockGeometry {
            logical_block_size: BlockSize::new(512).ok_or("block")?,
            physical_block_size: None,
            alignment_offset: None,
            minimum_io_size: None,
            optimal_io_size: None,
        }),
    });
    snapshot.graph.replace_existing(actual, node.clone())?;
    let observed = LiveObservation {
        reference: ObservationReference {
            probe_instance: Uuid::now_v7(),
            epoch: ProbeEpoch::new(1),
        },
        snapshot,
    };
    let binding = NodeCorrelation {
        source: planned,
        target: actual,
        identity: ExternalId::LvmLvUuid("new-lv".into()),
    };
    assert!(
        check_observed_goal(&run.context(), &observed, std::slice::from_ref(&binding))?
            .iter()
            .all(|check| check.status == LiveCheckStatus::Satisfied)
    );
    assert!(matches!(
        check_observed_goal(&run.context(), &observed, &[])?[0].status,
        LiveCheckStatus::Unknown { .. }
    ));
    for case in 0..5 {
        let mut changed = observed.clone();
        let mut lv = node.clone();
        match case {
            0 => lv.size.lvm_active = Some(false),
            1 => lv.size.lvm_active = None,
            2 => lv.size.block = None,
            3 => lv.kind.size = Some(Bytes::new(4096)),
            _ => lv.size.identities = vec![ExternalId::LvmLvUuid("replacement".into())],
        }
        changed.snapshot.graph.replace_existing(actual, lv)?;
        assert_ne!(
            check_observed_goal(&run.context(), &changed, std::slice::from_ref(&binding))?[0]
                .status,
            LiveCheckStatus::Satisfied,
            "case {case}"
        );
    }
    Ok(())
}
