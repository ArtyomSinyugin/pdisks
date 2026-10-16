//! Exact partition requests share the normal editor, interpreter, persistence, and freeze path.

use std::num::NonZeroU32;

use storage_core::model::{Bytes, FilesystemKind, NodeKind, PartitionAttributes, UsageRole};
use storage_engine::{
    HistoricalBaseline, PlanDocument,
    editing::{
        CorrelationError, DraftDocument, EditError, PlanEdit, PlanRequest, PlanSession,
        PreviewStatus, RebaseError,
    },
};
use storage_provider::{ActionPrerequisite, PartitionAction, ProviderAction};

/// Binary mebibyte used by the synthetic 64 MiB disk fixture.
const MIB: u64 = 1024 * 1024;

/// Exercises exact byte placement and dependent filesystem edits without native backends.
#[test]
fn partition_edits_replay_with_stable_ids_and_preserved_obligations()
-> Result<(), Box<dyn std::error::Error>> {
    let baseline: HistoricalBaseline =
        serde_json::from_str(include_str!("../examples/fixtures/partition_baseline.json"))?;
    let table = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::PartitionTable(_)).then_some(*id))
        .ok_or("missing fixture table")?;
    let mut session = PlanSession::new(baseline.clone())?;
    let receipt = session.edit(
        session.revision(),
        PlanEdit::Add(PlanRequest::CreatePartition {
            table,
            number: NonZeroU32::new(2).ok_or("zero partition number")?,
            offset: Bytes::new(16 * MIB),
            size: Bytes::new(16 * MIB),
            role: Some(UsageRole::Data),
            attributes: PartitionAttributes {
                gpt: Some(4),
                bootable: None,
            },
        }),
    )?;
    let operation = receipt.added_operation.ok_or("missing fixture value")?;
    let partition = receipt.created_node.ok_or("missing fixture value")?;
    let fs = session.add_filesystem(session.revision(), partition, FilesystemKind::Ext4)?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    let preview = session
        .preview()
        .calculated()
        .ok_or("missing fixture value")?;
    assert!(
        preview
            .request_coverage
            .iter()
            .all(|coverage| coverage.covered)
    );
    assert!(
        preview
            .report
            .unresolved_obligations
            .iter()
            .any(|obligation| matches!(
                obligation,
                storage_engine::SimulationObligation::Prerequisite {
                    prerequisite: ActionPrerequisite::ExtentAvailable { .. },
                    ..
                }
            ))
    );
    let frozen = session.freeze(session.revision())?;
    let frozen_json = frozen.document().canonical_json()?;
    PlanDocument::from_json(&frozen_json)?.analyse_against(&baseline)?;

    // The second partition must see the first one's simulated metadata in the shared prefix.
    let duplicate = session.edit(
        session.revision(),
        PlanEdit::Add(PlanRequest::CreatePartition {
            table,
            number: NonZeroU32::new(2).ok_or("zero partition number")?,
            offset: Bytes::new(40 * MIB),
            size: Bytes::new(MIB),
            role: None,
            attributes: PartitionAttributes::default(),
        }),
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    assert!(
        session
            .preview_document()
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.operation == duplicate.added_operation)
    );
    session.undo(session.revision())?;

    for (offset, size) in [(4 * MIB, MIB), (63 * MIB, 2 * MIB), (16 * MIB, 0)] {
        session.set_created_partition_extent(
            session.revision(),
            operation,
            Bytes::new(offset),
            Bytes::new(size),
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
        let saved = session.draft_document().to_json()?;
        let resumed = PlanSession::from_document(DraftDocument::from_json(&saved)?)?;
        assert_eq!(resumed.draft(), session.draft());
        assert_eq!(resumed.preview(), session.preview());
        session.undo(session.revision())?;
    }
    session.set_created_partition_extent(
        session.revision(),
        operation,
        Bytes::new(32 * MIB),
        Bytes::new(8 * MIB),
    )?;
    let revision = session.revision();
    let preview = session
        .preview()
        .calculated()
        .ok_or("missing fixture value")?;
    let node = preview
        .report
        .simulation
        .desired
        .graph
        .node(&partition)
        .ok_or("missing planned partition")?;
    assert_eq!(node.kind.size, Some(Bytes::new(8 * MIB)));
    assert!(
        matches!(node.kind.kind, NodeKind::Partition { offset, .. } if offset == Bytes::new(32 * MIB))
    );
    assert!(preview.plan.actions().iter().all(|action| !matches!(
        action.operation,
        ProviderAction::Partition(PartitionAction::Resize { .. })
    )));
    assert_eq!(session.draft().operations[0].created_node, Some(partition));
    session.undo(revision)?;
    session.redo(session.revision())?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    let before = session.draft_document();
    assert!(matches!(
        session.set_created_partition_extent(revision, operation, Bytes::new(0), Bytes::new(1)),
        Err(EditError::RevisionConflict { .. })
    ));
    assert_eq!(
        session.set_created_partition_extent(
            session.revision(),
            fs.added_operation.ok_or("missing fixture value")?,
            Bytes::new(0),
            Bytes::new(1)
        ),
        Err(EditError::NotCreatedPartition(
            fs.added_operation.ok_or("missing fixture value")?
        ))
    );
    assert_eq!(session.draft_document(), before);
    assert!(matches!(
        session.edit(
            session.revision(),
            PlanEdit::Remove {
                operations: vec![operation]
            }
        ),
        Err(EditError::DependentOperations { .. })
    ));
    let impact = session.removal_impact(operation)?;
    assert_eq!(
        impact,
        vec![
            operation,
            fs.added_operation.ok_or("missing fixture value")?
        ]
    );
    session.edit(session.revision(), PlanEdit::Remove { operations: impact })?;
    assert!(session.draft().operations.is_empty());
    assert_eq!(session.baseline(), &baseline);
    assert_eq!(frozen.document().canonical_json()?, frozen_json);
    // This fixture has no disk WWN, so its partition table still cannot be correlated.
    session.undo(session.revision())?;
    let mut next = baseline.clone();
    next.id = uuid::Uuid::now_v7();
    assert!(
        matches!(session.prepare_rebase(session.revision(), next), Err(RebaseError::Correlation(CorrelationError::NoIdentity(node))) if node == table)
    );
    assert_eq!(session.baseline(), &baseline);
    Ok(())
}
