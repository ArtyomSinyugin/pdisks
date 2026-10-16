//! Explicit rebase of a planned LV/FS/mount stack between two synthetic probe publications.
//! Run with `cargo run -p storage-engine --example rebase_plan`; no system calls or writes occur.

use std::error::Error;

use storage_core::model::{
    Bytes, FilesystemKind, MountContext, MountEntry, MountSource, NodeGraph, NodeId, NodeKind,
};
use storage_engine::{
    HistoricalBaseline,
    editing::{DraftDocument, EditError, PlanRequest, PlanSession, PreviewStatus, RebaseError},
};
use storage_probe::ProbeSnapshot;

/// Bytes in a binary gibibyte for exact synthetic requests.
const GIB: u64 = 1024 * 1024 * 1024;

/// Produces independent observation contexts sharing a VG UUID, with new transient ID and name.
fn observations() -> Result<(HistoricalBaseline, HistoricalBaseline, NodeId, NodeId), Box<dyn Error>>
{
    let old: ProbeSnapshot = serde_json::from_str(include_str!("fixtures/probe_snapshot.json"))?;
    let source = old.lvm_vgs[0].node;
    let original = HistoricalBaseline::from_lvm_observations(old.current.clone(), &old.lvm_vgs)?;
    let target = NodeId::new();
    let mut next = old;
    let mut node = next
        .current
        .graph
        .node(&source)
        .cloned()
        .ok_or("fixture VG missing")?;
    let NodeKind::LvmVg { name, .. } = &mut node.kind.kind else {
        return Err("fixture kind is not VG".into());
    };
    *name = "renamed-vg".to_owned();
    next.current.graph = NodeGraph::new();
    next.current.graph.insert_new(target, node)?;
    next.lvm_vgs[0].node = target;
    next.lvm_vgs[0].vg.name = "renamed-vg".to_owned();
    next.lvm_vgs[0].vg.free = 32 * GIB;
    let next = HistoricalBaseline::from_lvm_observations(next.current, &next.lvm_vgs)?;
    Ok((original, next, source, target))
}

/// Demonstrates review-before-publication, immutable frozen history, and editable invalid results.
fn main() -> Result<(), Box<dyn Error>> {
    let (original, next, source, target) = observations()?;
    let mut session = PlanSession::new(original)?;
    let receipt = session.add_lv(session.revision(), source, "data", Bytes::new(20 * GIB))?;
    let operation = receipt.added_operation.ok_or("LV request ID missing")?;
    let lv = receipt.created_node.ok_or("planned LV ID missing")?;
    let filesystem = session
        .add_filesystem(session.revision(), lv, FilesystemKind::Ext4)?
        .created_node
        .ok_or("planned filesystem ID missing")?;
    let entry = MountEntry {
        source: MountSource::Filesystem(filesystem),
        target: "/data".into(),
        options: vec!["rw".to_owned()],
        context: MountContext::Host,
    };
    session.mount(session.revision(), entry.clone())?;
    session.persist_mount(session.revision(), entry)?;
    let saved = session.draft_document();
    let frozen = session.freeze(session.revision())?;
    let frozen_json = frozen.document().canonical_json()?;

    let review = session.prepare_rebase(session.revision(), next.clone())?;
    assert_eq!(session.draft_document(), saved);
    assert_eq!(review.correlations().len(), 1);
    assert_eq!(
        (
            review.correlations()[0].source,
            review.correlations()[0].target
        ),
        (source, target)
    );
    assert_eq!(review.preview().status(), PreviewStatus::Conditional);
    assert!(
        matches!(review.draft().operations[0].request, PlanRequest::CreateLv { vg, .. } if vg == target)
    );
    for (old, new) in saved
        .draft
        .operations
        .iter()
        .zip(&review.draft().operations)
    {
        assert_eq!(old.id, new.id);
        assert_eq!(old.created_node, new.created_node);
    }
    println!(
        "review: VG UUID matched despite changed NodeId/name; capacity 64 -> 32 GiB; new preview {:?}",
        review.preview().status()
    );
    let replayed_review = review.clone();
    let old_revision = session.revision();
    session.accept_rebase(old_revision, review)?;
    assert!(session.revision() > old_revision);
    assert_eq!(session.baseline(), &next);
    assert_eq!(session.undo(session.revision()), Err(EditError::NoUndo));
    assert!(matches!(
        session.accept_rebase(session.revision(), replayed_review),
        Err(RebaseError::RevisionConflict { .. })
    ));
    assert_eq!(frozen.document().canonical_json()?, frozen_json);
    let resumed = PlanSession::from_document(DraftDocument::from_json(
        &session.draft_document().to_json()?,
    )?)?;
    assert_eq!(resumed.baseline(), &next);
    assert_eq!(resumed.draft(), session.draft());
    assert_eq!(resumed.preview(), session.preview());

    let mut measured: ProbeSnapshot =
        serde_json::from_str(include_str!("fixtures/probe_snapshot.json"))?;
    measured.current = session.baseline().snapshot.clone();
    measured.lvm_vgs[0].node = target;
    measured.lvm_vgs[0].vg.name = "renamed-vg".to_owned();
    measured.lvm_vgs[0].vg.free = GIB;
    let insufficient =
        HistoricalBaseline::from_lvm_observations(measured.current, &measured.lvm_vgs)?;
    let review = session.prepare_rebase(session.revision(), insufficient)?;
    assert_eq!(review.preview().status(), PreviewStatus::Invalid);
    assert!(review.preview().calculated().is_none());
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    println!(
        "review: capacity fell to 1 GiB; proposed preview Invalid; active preview still Conditional"
    );
    session.accept_rebase(session.revision(), review)?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    assert!(session.freeze(session.revision()).is_err());
    session.set_created_lv_size(session.revision(), operation, Bytes::new(GIB))?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    assert_eq!(session.draft().operations[0].created_node, Some(lv));
    assert_eq!(session.draft().operations[1].created_node, Some(filesystem));
    session.undo(session.revision())?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    assert_eq!(session.baseline().capacity_evidence[0].available.get(), 256);
    assert_eq!(frozen.document().canonical_json()?, frozen_json);
    println!(
        "accepted invalid candidate remains editable; repair/undo keep the new baseline; old frozen plan is unchanged"
    );
    Ok(())
}
