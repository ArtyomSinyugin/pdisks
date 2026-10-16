//! Offline resize of an existing mounted ext4 filesystem on a linear LV.
//! Run with `cargo run -p storage-engine --example resize_plan`; no probe or device writes.

use std::error::Error;
use storage_core::model::{
    Bytes, MountContext, MountEntry, MountSource, NodeKind, OperationIntent,
};
use storage_engine::{
    HistoricalBaseline, PlanDocument, SimulationObligation,
    editing::{DraftDocument, PlanEdit, PlanRequest, PlanSession, PreviewStatus},
};
use storage_provider::{ActionPrerequisite, AllocationDomain};

/// Binary mebibyte used by the synthetic existing-storage fixture.
const MIB: u64 = 1024 * 1024;

/// Demonstrates explicit resize order, required shrink preparation, edits, and immutable history.
fn main() -> Result<(), Box<dyn Error>> {
    let baseline: HistoricalBaseline =
        serde_json::from_str(include_str!("fixtures/resize_baseline.json"))?;
    let lv = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::LvmLv(_)).then_some(*id))
        .ok_or("LV missing")?;
    let filesystem = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::Filesystem { .. }).then_some(*id))
        .ok_or("filesystem missing")?;
    let AllocationDomain::LvmVolumeGroup(vg) = baseline.capacity_evidence[0].domain;
    let mut session = PlanSession::new(baseline.clone())?;
    for request in [
        PlanRequest::ResizeLv {
            lv,
            new_size: Bytes::new(160 * MIB),
        },
        PlanRequest::ResizeFilesystem {
            filesystem,
            new_size: Bytes::new(160 * MIB),
        },
    ] {
        session.edit(session.revision(), PlanEdit::Add(request))?;
    }
    let grown = session.freeze(session.revision())?;
    let grown_json = grown.document().canonical_json()?;
    PlanDocument::from_json(&grown_json)?.analyse_against(&baseline)?;
    println!("existing LV + mounted ext4: 128 -> 160 MiB, backing grows first; no new storage IDs");

    for request in [
        PlanRequest::Unmount {
            target: "/data".into(),
            context: MountContext::Host,
        },
        PlanRequest::Maintenance(OperationIntent::Check {
            target: filesystem,
            force: true,
        }),
        PlanRequest::ResizeFilesystem {
            filesystem,
            new_size: Bytes::new(96 * MIB),
        },
    ] {
        session.edit(session.revision(), PlanEdit::Add(request))?;
    }
    let shrink_lv = session
        .edit(
            session.revision(),
            PlanEdit::Add(PlanRequest::ResizeLv {
                lv,
                new_size: Bytes::new(96 * MIB),
            }),
        )?
        .added_operation
        .ok_or("LV resize operation missing")?;
    session.mount(
        session.revision(),
        MountEntry {
            source: MountSource::Filesystem(filesystem),
            target: "/data".into(),
            options: vec!["rw".to_owned()],
            context: MountContext::Host,
        },
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    let preview = session
        .preview()
        .calculated()
        .ok_or("shrink preview missing")?;
    for node in [lv, filesystem] {
        assert_eq!(
            preview
                .report
                .simulation
                .desired
                .graph
                .node(&node)
                .ok_or("resized node missing")?
                .kind
                .size,
            Some(Bytes::new(96 * MIB))
        );
    }
    assert!(
        preview
            .report
            .unresolved_obligations
            .iter()
            .any(|obligation| matches!(
                obligation,
                SimulationObligation::Prerequisite {
                    prerequisite: ActionPrerequisite::FilesystemMinimumSize { .. },
                    ..
                }
            ))
    );
    let saved = session.draft_document().to_json()?;
    let resumed = PlanSession::from_document(DraftDocument::from_json(&saved)?)?;
    assert_eq!(resumed.draft(), session.draft());
    assert_eq!(resumed.preview(), session.preview());
    let shrunk = session.freeze(session.revision())?;
    PlanDocument::from_json(&shrunk.document().canonical_json()?)?.analyse_against(&baseline)?;
    println!("160 -> 96 MiB: unmount -> forced check -> filesystem shrink -> LV shrink -> mount");
    println!(
        "safe filesystem minimum remains a live obligation; offline freeze grants no write permission"
    );

    session.set_resize_size(session.revision(), shrink_lv, Bytes::new(64 * MIB))?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    assert!(session.freeze(session.revision()).is_err());
    session.undo(session.revision())?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    // Baseline free 64 MiB + net shrink 32 MiB permits exactly 96 MiB of new allocation.
    session.add_lv(session.revision(), vg, "reuse", Bytes::new(96 * MIB))?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    session.add_lv(session.revision(), vg, "too-much", Bytes::new(4 * MIB))?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    session.undo(session.revision())?;
    assert_eq!(session.baseline(), &baseline);
    assert_eq!(grown.document().canonical_json()?, grown_json);
    println!(
        "unsafe LV shrink rejected; undo repaired it; net freed capacity counted exactly once"
    );
    Ok(())
}
