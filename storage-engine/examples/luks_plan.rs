//! Offline preparation of an encrypted filesystem from synthetic VG capacity.
//! Run with `cargo run -p storage-engine --example luks_plan`; no secrets, probe, or writes.

use std::error::Error;

use storage_core::model::{
    Bytes, FilesystemKind, LuksVersion, MountContext, MountEntry, MountSource,
};
use storage_engine::{
    HistoricalBaseline, PlanDocument, SimulationObligation,
    editing::{DraftDocument, PlanEdit, PlanRequest, PlanSession, PreviewStatus},
};
use storage_provider::{ActionPrerequisite, AllocationDomain, CredentialRef};

/// Builds, persists, and freezes an encrypted stack while retaining unresolved live checks.
fn main() -> Result<(), Box<dyn Error>> {
    let baseline: HistoricalBaseline =
        serde_json::from_str(include_str!("fixtures/editing_baseline.json"))?;
    let AllocationDomain::LvmVolumeGroup(vg) = baseline.capacity_evidence[0].domain;
    let mut session = PlanSession::new(baseline.clone())?;
    // This fixture UUID names no real secret; a future runtime must resolve it externally.
    let credential = CredentialRef::from_uuid(uuid::Uuid::parse_str(
        "00000000-0000-7000-8000-000000000301",
    )?);
    let lv = session
        .add_lv(
            session.revision(),
            vg,
            "encrypted",
            Bytes::new(64 * 1024 * 1024),
        )?
        .created_node
        .ok_or("LV output missing")?;
    let receipt = session.edit(
        session.revision(),
        PlanEdit::Add(PlanRequest::FormatLuks {
            backing: lv,
            version: LuksVersion::Luks2,
            credential,
        }),
    )?;
    let format = receipt.added_operation.ok_or("format operation missing")?;
    let container = receipt.created_node.ok_or("container output missing")?;
    let receipt = session.edit(
        session.revision(),
        PlanEdit::Add(PlanRequest::OpenLuks {
            container,
            mapping_name: "crypt-data".to_owned(),
            credential,
            read_only: false,
        }),
    )?;
    let open = receipt.added_operation.ok_or("open operation missing")?;
    let mapping = receipt.created_node.ok_or("mapping output missing")?;
    let filesystem = session
        .add_filesystem(session.revision(), mapping, FilesystemKind::Ext4)?
        .created_node
        .ok_or("filesystem output missing")?;
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
    let preview = session.preview().calculated().ok_or("preview missing")?;
    assert!(
        preview
            .request_coverage
            .iter()
            .all(|coverage| coverage.covered)
    );
    assert_eq!(
        preview
            .report
            .unresolved_obligations
            .iter()
            .filter(|obligation| matches!(
                obligation,
                SimulationObligation::Prerequisite {
                    prerequisite: ActionPrerequisite::CredentialAvailable(_),
                    ..
                }
            ))
            .count(),
        2
    );
    for node in [container, mapping, filesystem] {
        assert_eq!(
            preview
                .report
                .simulation
                .desired
                .graph
                .node(&node)
                .ok_or("desired node missing")?
                .kind
                .size,
            None
        );
    }
    let saved = session.draft_document().to_json()?;
    let resumed = PlanSession::from_document(DraftDocument::from_json(&saved)?)?;
    assert_eq!(resumed.draft(), session.draft());
    assert_eq!(resumed.preview(), session.preview());
    let frozen = session.freeze(session.revision())?;
    let json = frozen.document().canonical_json()?;
    let analysed = PlanDocument::from_json(&json)?.analyse_against(&baseline)?;
    assert!(!analysed.report.unresolved_obligations.is_empty());
    println!("64 MiB LV -> LUKS2 -> crypt-data -> ext4 -> /data: saved and frozen conditionally");
    println!(
        "credential resolution and destructive approval remain live obligations; payload size is unknown"
    );

    let mut request = session
        .draft()
        .operations
        .iter()
        .find(|operation| operation.id == open)
        .ok_or("open request missing")?
        .request
        .clone();
    if let PlanRequest::OpenLuks { read_only, .. } = &mut request {
        *read_only = true;
    }
    session.edit(
        session.revision(),
        PlanEdit::Replace {
            operation: open,
            request,
        },
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    assert!(session.freeze(session.revision()).is_err());
    session.undo(session.revision())?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    let impact = session.removal_impact(format)?;
    assert_eq!(impact.len(), 4);
    session.edit(session.revision(), PlanEdit::Remove { operations: impact })?;
    assert_eq!(session.draft().operations.len(), 1);
    assert_eq!(session.draft().operations[0].created_node, Some(lv));
    assert_eq!(session.baseline(), &baseline);
    assert_eq!(frozen.document().canonical_json()?, json);
    println!(
        "filesystem creation through read-only mapping rejected; undo restored it; crypto requests cancelled, LV retained"
    );
    Ok(())
}
