//! Offline plan editing from a synthetic VG fixture with 64 GiB free in 4 MiB extents.
//! Run with `cargo run -p storage-engine --example edit_plan`; no probing or writes occur.

use std::error::Error;

use storage_core::model::{
    Bytes, DesiredState, FilesystemKind, MountContext, MountEntry, MountSource,
};
use storage_engine::{
    HistoricalBaseline, PlanDocument,
    editing::{DraftDocument, EditError, PlanEdit, PlanSession, PreviewStatus},
};
use storage_provider::{AllocationDomain, LvmAction, ProviderAction};

/// Bytes in one binary gibibyte, used only for the synthetic exact-size requests.
const GIB: u64 = 1024 * 1024 * 1024;

/// Exercises stable identities, invalid edits, persistence, freezing, and explicit cancellation.
fn main() -> Result<(), Box<dyn Error>> {
    let baseline: HistoricalBaseline =
        serde_json::from_str(include_str!("fixtures/editing_baseline.json"))?;
    let AllocationDomain::LvmVolumeGroup(vg) = baseline.capacity_evidence[0].domain;
    let mut session = PlanSession::new(baseline.clone())?;
    assert_eq!(
        session
            .preview()
            .calculated()
            .map(|preview| &preview.report.simulation.desired),
        Some(&DesiredState::from_current(&baseline.snapshot))
    );

    let receipt = session.add_lv(session.revision(), vg, "data", Bytes::new(20 * GIB))?;
    let create_lv = receipt.added_operation.ok_or("LV operation ID missing")?;
    let lv = receipt.created_node.ok_or("LV node ID missing")?;
    let receipt = session.add_filesystem(session.revision(), lv, FilesystemKind::Ext4)?;
    let filesystem = receipt.created_node.ok_or("filesystem node ID missing")?;
    let entry = MountEntry {
        source: MountSource::Filesystem(filesystem),
        target: "/data".into(),
        options: vec!["rw".to_owned()],
        context: MountContext::Host,
    };
    session.mount(session.revision(), entry.clone())?;
    session.persist_mount(session.revision(), entry)?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    println!(
        "revision {}: 20 GiB LV + ext4 + runtime/persistent mounts, conditional preview",
        session.revision().get()
    );

    session.set_created_lv_size(session.revision(), create_lv, Bytes::new(30 * GIB))?;
    assert_eq!(session.draft().operations[0].created_node, Some(lv));
    assert_eq!(session.draft().operations[1].created_node, Some(filesystem));
    let preview = session
        .preview()
        .calculated()
        .ok_or("30 GiB preview missing")?;
    assert!(preview.plan.actions().iter().all(|action| !matches!(
        action.operation,
        ProviderAction::Lvm(LvmAction::ResizeLv { .. })
    )));
    let frozen = session.freeze(session.revision())?;
    let frozen_json = frozen.document().canonical_json()?;
    assert_eq!(frozen.document().canonical_json()?, frozen_json);
    assert_eq!(frozen.document().baseline.id, baseline.id);
    let analysed = PlanDocument::from_json(&frozen_json)?.analyse_against(&baseline)?;
    assert!(!analysed.report.unresolved_obligations.is_empty());

    let saved = session.draft_document().to_json()?;
    let resumed = PlanSession::from_document(DraftDocument::from_json(&saved)?)?;
    assert_eq!(resumed.draft(), session.draft());
    assert_eq!(resumed.revision(), session.revision());
    assert_eq!(resumed.id(), session.id());
    assert_eq!(resumed.preview(), session.preview());
    println!(
        "revision {}: 30 GiB create request saved, resumed and frozen; planned node IDs unchanged",
        session.revision().get()
    );

    let before_undo = session.revision();
    session.undo(before_undo)?;
    assert!(session.revision() > before_undo);
    session.redo(session.revision())?;
    session.set_created_lv_size(session.revision(), create_lv, Bytes::new(100 * GIB))?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    assert!(session.preview().calculated().is_none());
    let invalid = session.draft_document().to_json()?;
    assert_eq!(
        PlanSession::from_document(DraftDocument::from_json(&invalid)?)?
            .preview()
            .status(),
        PreviewStatus::Invalid
    );
    assert_eq!(
        session.preview_document().diagnostics[0].operation,
        Some(create_lv)
    );
    assert_eq!(session.baseline(), &baseline);
    session.undo(session.revision())?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    assert_eq!(frozen.document().canonical_json()?, frozen_json);

    let impact = session.removal_impact(create_lv)?;
    assert_eq!(impact.len(), 4);
    assert!(matches!(
        session.edit(
            session.revision(),
            PlanEdit::Remove {
                operations: vec![create_lv]
            }
        ),
        Err(EditError::DependentOperations { .. })
    ));
    session.edit(session.revision(), PlanEdit::Remove { operations: impact })?;
    assert!(session.draft().operations.is_empty());
    session.undo(session.revision())?;
    session.redo(session.revision())?;
    session.undo(session.revision())?;
    session.edit(session.revision(), PlanEdit::Clear)?;
    assert_eq!(session.redo(session.revision()), Err(EditError::NoRedo));
    assert_eq!(session.baseline(), &baseline);
    assert_eq!(
        session
            .preview()
            .calculated()
            .map(|preview| &preview.report.simulation.desired),
        Some(&DesiredState::from_current(&baseline.snapshot))
    );
    println!(
        "revision {}: invalid size corrected with undo; dependent requests cancelled; baseline unchanged",
        session.revision().get()
    );
    Ok(())
}
