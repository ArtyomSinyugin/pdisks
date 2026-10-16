//! Synthetic observation checks for a frozen label edit; no probe, leases, or backend calls.
//! Run with `cargo run -p storage-engine --example observed_checks`.

use std::error::Error;
use storage_core::model::{NodeId, NodeKind, ProbeEpoch};
use storage_engine::{
    ActionId, ActionRunState, ExecutionRun, HistoricalBaseline, LiveCheckStatus, LiveObservation,
    ObservationReference,
    editing::{PlanEdit, PlanRequest, PlanSession},
    live::check_observed_prerequisites,
};
use storage_provider::ActionPrerequisite;
use uuid::Uuid;

/// Shows that a historically valid label edit cannot override a new read-only endpoint.
fn main() -> Result<(), Box<dyn Error>> {
    let baseline: HistoricalBaseline =
        serde_json::from_str(include_str!("fixtures/resize_baseline.json"))?;
    let lv = *baseline
        .snapshot
        .graph
        .nodes()
        .find(|(_, node)| matches!(node.kind.kind, NodeKind::LvmLv(_)))
        .ok_or("fixture LV missing")?
        .0;
    let mut session = PlanSession::new(baseline.clone())?;
    let filesystem = *baseline
        .snapshot
        .graph
        .nodes()
        .find(|(_, node)| matches!(node.kind.kind, NodeKind::Filesystem { .. }))
        .ok_or("fixture filesystem missing")?
        .0;
    session.edit(
        session.revision(),
        PlanEdit::Add(PlanRequest::SetFilesystemLabel {
            filesystem,
            label: Some("renamed".into()),
        }),
    )?;
    let frozen = session.freeze(session.revision())?;
    let original = frozen.document().canonical_json()?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let action = ActionId::new(0);
    run.begin_check(action)?;

    // This second snapshot is explicitly a fixture, never published as an actual live probe.
    let renumbered = NodeId::new();
    let mut observation = LiveObservation {
        reference: ObservationReference {
            probe_instance: Uuid::now_v7(),
            epoch: ProbeEpoch::new(1),
        },
        snapshot: serde_json::from_str(
            &serde_json::to_string(&baseline.snapshot)?
                .replace(&lv.as_uuid().to_string(), &renumbered.as_uuid().to_string()),
        )?,
    };
    let mut endpoint = observation
        .snapshot
        .graph
        .node(&renumbered)
        .ok_or("new endpoint")?
        .clone();
    endpoint.size.block.as_mut().ok_or("block facts")?.read_only = Some(true);
    observation
        .snapshot
        .graph
        .replace_existing(renumbered, endpoint)?;
    let report = check_observed_prerequisites(
        &baseline,
        &observation,
        &run.context().plan.actions()[0]
            .informational_contract
            .prerequisites,
    )?;
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.condition == ActionPrerequisite::Writable(lv)
                && matches!(check.status, LiveCheckStatus::Violated { .. }))
    );
    assert_eq!(run.state(action)?, ActionRunState::Checking);
    assert!(run.latest_observation().is_none());
    assert_eq!(frozen.document().canonical_json()?, original);
    println!("synthetic fixture only: UUID correlation survives a changed NodeId");
    println!("{}", serde_json::to_string_pretty(&report)?);
    println!("fresh fixture read-only status vetoes the label edit; run remains Checking");
    println!(
        "actual probe, applicability, authorization, leases, and native dispatch remain unconfigured"
    );
    Ok(())
}
