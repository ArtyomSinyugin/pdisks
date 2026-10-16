//! Offline exact partition placement on a synthetic disk; no probe or device writes occur.
//! Run with `cargo run -p storage-engine --example partition_plan`.

use std::{error::Error, num::NonZeroU32};

use storage_core::model::{
    Bytes, FilesystemKind, MountContext, MountEntry, MountSource, NodeKind, PartitionAttributes,
    UsageRole,
};
use storage_engine::{
    HistoricalBaseline, PlanDocument,
    editing::{DraftDocument, PlanEdit, PlanRequest, PlanSession, PreviewStatus},
};

/// Binary mebibyte used for explicit byte geometry on the synthetic 64 MiB disk.
const MIB: u64 = 1024 * 1024;

/// Demonstrates exact placement, dependent requests, invalid edits, persistence, and freezing.
fn main() -> Result<(), Box<dyn Error>> {
    let baseline: HistoricalBaseline =
        serde_json::from_str(include_str!("fixtures/partition_baseline.json"))?;
    let table = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::PartitionTable(_)).then_some(*id))
        .ok_or("partition table missing")?;
    let mut session = PlanSession::new(baseline.clone())?;
    let receipt = session.edit(
        session.revision(),
        PlanEdit::Add(PlanRequest::CreatePartition {
            table,
            number: NonZeroU32::new(2).ok_or("partition number must be nonzero")?,
            offset: Bytes::new(16 * MIB),
            size: Bytes::new(16 * MIB),
            role: Some(UsageRole::Data),
            attributes: PartitionAttributes::default(),
        }),
    )?;
    let operation = receipt
        .added_operation
        .ok_or("partition operation missing")?;
    let partition = receipt.created_node.ok_or("partition output missing")?;
    let filesystem = session
        .add_filesystem(session.revision(), partition, FilesystemKind::Ext4)?
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
    session.set_created_partition_extent(
        session.revision(),
        operation,
        Bytes::new(32 * MIB),
        Bytes::new(8 * MIB),
    )?;
    assert_eq!(session.draft().operations[0].created_node, Some(partition));
    let saved = session.draft_document().to_json()?;
    let resumed = PlanSession::from_document(DraftDocument::from_json(&saved)?)?;
    assert_eq!(resumed.draft(), session.draft());
    assert_eq!(resumed.preview(), session.preview());
    let frozen = session.freeze(session.revision())?;
    let json = frozen.document().canonical_json()?;
    let analysed = PlanDocument::from_json(&json)?.analyse_against(&baseline)?;
    assert!(!analysed.report.unresolved_obligations.is_empty());
    println!(
        "partition #2: offset 32 MiB, size 8 MiB; ext4 + mount; saved and frozen conditionally"
    );

    // The fixture's first partition occupies bytes [1 MiB, 9 MiB).
    session.set_created_partition_extent(
        session.revision(),
        operation,
        Bytes::new(4 * MIB),
        Bytes::new(8 * MIB),
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    assert!(session.freeze(session.revision()).is_err());
    assert!(
        session
            .preview_document()
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.operation == Some(operation))
    );
    session.undo(session.revision())?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    assert_eq!(frozen.document().canonical_json()?, json);
    let impact = session.removal_impact(operation)?;
    assert_eq!(impact.len(), 3);
    session.edit(session.revision(), PlanEdit::Remove { operations: impact })?;
    assert!(session.draft().operations.is_empty());
    assert_eq!(session.baseline(), &baseline);
    println!("overlap rejected; undo restored placement; partition + dependent requests cancelled");
    Ok(())
}
