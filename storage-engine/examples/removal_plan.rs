//! Offline removal of three explicitly selected fixture objects; no probing or writes occur.
//! Run with `cargo run -p storage-engine --example removal_plan`.

use std::error::Error;
use storage_core::model::{Bytes, NodeKind};
use storage_engine::{
    HistoricalBaseline, PlanDocument, SimulationObligation,
    editing::{DraftDocument, PlanEdit, PlanRequest, PlanSession, PreviewStatus},
};
use storage_provider::{ActionPrerequisite, AllocationDomain};

/// Demonstrates exact removal scope, retained neighbors, invalid edits, and immutable frozen history.
fn main() -> Result<(), Box<dyn Error>> {
    let baseline: HistoricalBaseline =
        serde_json::from_str(include_str!("fixtures/removal_baseline.json"))?;
    // Names/numbers identify this synthetic fixture only; requests themselves retain typed NodeIds.
    let partition = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| {
            matches!(node.kind.kind, NodeKind::Partition { number, .. } if number.get() == 1)
                .then_some(*id)
        })
        .ok_or("partition missing")?;
    let spare = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| {
            matches!(&node.kind.kind, NodeKind::LvmLv(value) if value.name == "spare")
                .then_some(*id)
        })
        .ok_or("spare LV missing")?;
    let busy = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| {
            matches!(&node.kind.kind, NodeKind::LvmLv(value) if value.name == "data").then_some(*id)
        })
        .ok_or("data LV missing")?;
    let mapping = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| {
            matches!(node.kind.kind, NodeKind::DmCryptMapping { .. }).then_some(*id)
        })
        .ok_or("mapping missing")?;
    let mut session = PlanSession::new(baseline.clone())?;
    session.edit(
        session.revision(),
        PlanEdit::Add(PlanRequest::DeletePartition { partition }),
    )?;
    let remove_lv = session
        .edit(
            session.revision(),
            PlanEdit::Add(PlanRequest::RemoveLv { lv: spare }),
        )?
        .added_operation
        .ok_or("remove LV operation missing")?;
    session.edit(
        session.revision(),
        PlanEdit::Add(PlanRequest::CloseLuks { mapping }),
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    let preview = session.preview().calculated().ok_or("preview missing")?;
    assert!(
        preview
            .request_coverage
            .iter()
            .all(|coverage| coverage.covered)
    );
    let targets = [partition, spare, mapping];
    for (id, node) in baseline.snapshot.graph.nodes() {
        assert_eq!(
            preview.report.simulation.desired.graph.node(id),
            if targets.contains(id) {
                None
            } else {
                Some(node)
            }
        );
    }
    for target in [partition, spare] {
        assert!(preview.report.unresolved_obligations.iter().any(|obligation| matches!(obligation,
            SimulationObligation::Prerequisite { prerequisite: ActionPrerequisite::DestructiveApproval(node), .. } if *node == target)));
    }
    let saved = session.draft_document().to_json()?;
    let resumed = PlanSession::from_document(DraftDocument::from_json(&saved)?)?;
    assert_eq!(resumed.draft(), session.draft());
    assert_eq!(resumed.preview(), session.preview());
    let frozen = session.freeze(session.revision())?;
    let json = frozen.document().canonical_json()?;
    PlanDocument::from_json(&json)?.analyse_against(&baseline)?;
    println!(
        "scope: partition #1, spare LV, unused mapping; table, sibling partition/LVs, LUKS container and mounted data retained"
    );
    println!(
        "deletion approvals remain live obligations; saved/frozen plans perform no disk operations"
    );

    session.edit(
        session.revision(),
        PlanEdit::Replace {
            operation: remove_lv,
            request: PlanRequest::RemoveLv { lv: busy },
        },
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    assert!(session.freeze(session.revision()).is_err());
    session.undo(session.revision())?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    // Cancelling the request restores the selected LV only in the forecast.
    session.edit(
        session.revision(),
        PlanEdit::Remove {
            operations: vec![remove_lv],
        },
    )?;
    assert!(
        session
            .preview()
            .calculated()
            .ok_or("cancel preview missing")?
            .report
            .simulation
            .desired
            .graph
            .node(&spare)
            .is_some()
    );
    session.undo(session.revision())?;
    let AllocationDomain::LvmVolumeGroup(vg) = baseline.capacity_evidence[0].domain;
    session.add_lv(
        session.revision(),
        vg,
        "reuse",
        Bytes::new(80 * 1024 * 1024),
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    session.add_lv(
        session.revision(),
        vg,
        "too-much",
        Bytes::new(4 * 1024 * 1024),
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    session.undo(session.revision())?;
    assert_eq!(session.baseline(), &baseline);
    assert_eq!(frozen.document().canonical_json()?, json);
    println!(
        "busy LV rejected without deleting its filesystem; cancellation/undo preserved scope; 16 MiB freed once"
    );
    Ok(())
}
