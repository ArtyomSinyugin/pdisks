//! Explicit removal requests preserve unrelated objects and require fresh destructive-scope checks.

use std::error::Error;
use storage_core::model::{NodeId, NodeKind};
use storage_engine::{
    HistoricalBaseline,
    editing::{PlanEdit, PlanRequest, PlanSession, PreviewStatus},
};

/// Loads synthetic independent partitions, LVs, and a LUKS mapping alongside a mounted data LV.
fn baseline() -> Result<HistoricalBaseline, Box<dyn Error>> {
    Ok(serde_json::from_str(include_str!(
        "../examples/fixtures/removal_baseline.json"
    ))?)
}

/// Selects exactly three fixture objects; production callers supply their reviewed NodeIds.
fn requests(baseline: &HistoricalBaseline) -> Result<[PlanRequest; 3], Box<dyn Error>> {
    let partition = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| {
            matches!(node.kind.kind, NodeKind::Partition { number, .. } if number.get() == 1)
                .then_some(*id)
        })
        .ok_or("missing partition")?;
    let lv = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| {
            matches!(&node.kind.kind, NodeKind::LvmLv(value) if value.name == "spare")
                .then_some(*id)
        })
        .ok_or("missing spare LV")?;
    let mapping = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| {
            matches!(node.kind.kind, NodeKind::DmCryptMapping { .. }).then_some(*id)
        })
        .ok_or("missing mapping")?;
    Ok([
        PlanRequest::DeletePartition { partition },
        PlanRequest::RemoveLv { lv },
        PlanRequest::CloseLuks { mapping },
    ])
}

/// Only explicitly selected nodes disappear; cancelling/undoing a request restores the prediction.
#[test]
fn selected_removals_preserve_neighbors_history_and_capacity() -> Result<(), Box<dyn Error>> {
    use storage_core::model::Bytes;
    use storage_engine::{PlanDocument, SimulationObligation, editing::DraftDocument};
    use storage_provider::{ActionPrerequisite, AllocationDomain};
    let baseline = baseline()?;
    let selected = requests(&baseline)?;
    let targets: Vec<_> = selected
        .iter()
        .map(|request| request.referenced_nodes()[0])
        .collect();
    let mut session = PlanSession::new(baseline.clone())?;
    for request in selected {
        session.edit(session.revision(), PlanEdit::Add(request))?;
    }
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    let preview = session.preview().calculated().ok_or("missing preview")?;
    assert!(
        preview
            .request_coverage
            .iter()
            .all(|coverage| coverage.covered)
    );
    assert_eq!(preview.plan.actions().len(), 3);
    assert_eq!(
        preview.report.simulation.desired.graph.nodes().len(),
        baseline.snapshot.graph.nodes().len() - 3
    );
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
    assert_eq!(
        preview.report.simulation.desired.mounts.runtime.len(),
        baseline.snapshot.mounts.entries.len()
    );
    for target in &targets[..2] {
        assert!(preview.report.unresolved_obligations.iter().any(|obligation| matches!(obligation,
            SimulationObligation::Prerequisite { prerequisite: ActionPrerequisite::DestructiveApproval(node), .. } if node == target)));
    }
    let saved = session.draft_document().to_json()?;
    let resumed = PlanSession::from_document(DraftDocument::from_json(&saved)?)?;
    assert_eq!(resumed.preview(), session.preview());
    assert_eq!(resumed.draft_document(), session.draft_document());
    let frozen = session.freeze(session.revision())?;
    let json = frozen.document().canonical_json()?;
    PlanDocument::from_json(&json)?.analyse_against(&baseline)?;
    let operation = session.draft().operations[0].id;
    session.edit(
        session.revision(),
        PlanEdit::Remove {
            operations: vec![operation],
        },
    )?;
    assert!(
        session
            .preview()
            .calculated()
            .ok_or("missing cancel preview")?
            .report
            .simulation
            .desired
            .graph
            .node(&targets[0])
            .is_some()
    );
    session.undo(session.revision())?;
    session.redo(session.revision())?;
    session.undo(session.revision())?;
    assert_eq!(session.draft().operations[0].id, operation);
    // Free 64 MiB plus the selected 16 MiB LV, without freeing its siblings.
    let AllocationDomain::LvmVolumeGroup(vg) = baseline.capacity_evidence[0].domain;
    session.add_lv(
        session.revision(),
        vg,
        "reuse",
        Bytes::new(80 * 1024 * 1024),
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    let excess = session
        .add_lv(
            session.revision(),
            vg,
            "excess",
            Bytes::new(4 * 1024 * 1024),
        )?
        .added_operation;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    assert!(
        session
            .preview_document()
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.operation == excess)
    );
    session.undo(session.revision())?;
    assert_eq!(frozen.document().canonical_json()?, json);
    assert_eq!(session.baseline(), &baseline);
    Ok(())
}

/// Remaining consumers/relations, wrong kinds, and repeated deletion cannot broaden scope.
#[test]
fn unsafe_removals_are_attributed_saved_and_never_frozen() -> Result<(), Box<dyn Error>> {
    use storage_core::model::{
        Dependency, DependencyKind, FilesystemKind, Node, NodeFacts, NodeSpec, Relation,
        RelationKind,
    };
    use storage_engine::{ProtectedScope, editing::DraftDocument};
    let baseline = baseline()?;
    for request in requests(&baseline)? {
        let target = request.referenced_nodes()[0];
        for related in [false, true] {
            let mut observed = baseline.clone();
            let consumer = NodeId::new();
            observed.snapshot.graph.insert_new(
                consumer,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Filesystem {
                            kind: FilesystemKind::Ext4,
                            label: None,
                        },
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )?;
            if related {
                observed.snapshot.graph.insert_relation(Relation {
                    from: consumer,
                    to: target,
                    kind: RelationKind::SnapshotOf,
                });
            } else {
                observed.snapshot.graph.insert_dependency(Dependency {
                    from: target,
                    to: consumer,
                    kind: DependencyKind::Backs,
                });
            }
            let mut session = PlanSession::new(observed.clone())?;
            let operation = session
                .edit(session.revision(), PlanEdit::Add(request.clone()))?
                .added_operation;
            assert_eq!(session.preview().status(), PreviewStatus::Invalid);
            assert!(session.freeze(session.revision()).is_err());
            assert!(
                session
                    .preview_document()
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.operation == operation)
            );
            let saved = session.draft_document().to_json()?;
            let resumed = PlanSession::from_document(DraftDocument::from_json(&saved)?)?;
            assert_eq!(resumed.preview(), session.preview());
            assert_eq!(session.baseline(), &observed);
        }
        let mut protected = PlanSession::new(baseline.clone())?;
        protected.edit(
            protected.revision(),
            PlanEdit::SetProtection(ProtectedScope {
                nodes: vec![target],
                ..ProtectedScope::default()
            }),
        )?;
        protected.edit(protected.revision(), PlanEdit::Add(request.clone()))?;
        assert!(matches!(
            protected.preview().status(),
            PreviewStatus::Invalid | PreviewStatus::Blocked
        ));
        assert!(protected.freeze(protected.revision()).is_err());
        let mut duplicate = PlanSession::new(baseline.clone())?;
        duplicate.edit(duplicate.revision(), PlanEdit::Add(request.clone()))?;
        let receipt = duplicate.edit(duplicate.revision(), PlanEdit::Add(request))?;
        assert_eq!(duplicate.preview().status(), PreviewStatus::Invalid);
        assert!(
            duplicate
                .preview_document()
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.operation == receipt.added_operation)
        );
    }
    let selected = requests(&baseline)?;
    let partition = selected[0].referenced_nodes()[0];
    let lv = selected[1].referenced_nodes()[0];
    for request in [
        PlanRequest::DeletePartition { partition: lv },
        PlanRequest::RemoveLv { lv: partition },
        PlanRequest::CloseLuks { mapping: lv },
    ] {
        let mut session = PlanSession::new(baseline.clone())?;
        session.edit(session.revision(), PlanEdit::Add(request))?;
        assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    }
    let mut session = PlanSession::new(baseline.clone())?;
    let busy_lv = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| {
            matches!(&node.kind.kind, NodeKind::LvmLv(value) if value.name == "data").then_some(*id)
        })
        .ok_or("missing busy LV")?;
    session.edit(
        session.revision(),
        PlanEdit::Add(PlanRequest::RemoveLv { lv: busy_lv }),
    )?;
    assert_eq!(
        session.preview().status(),
        PreviewStatus::Invalid,
        "an existing filesystem cannot be implicitly deleted"
    );
    Ok(())
}

/// Rebase correlates the selected LV by UUID, then checks its newly observed consumers again.
#[test]
fn rebased_removal_does_not_bypass_new_consumers() -> Result<(), Box<dyn Error>> {
    use storage_core::model::{
        Dependency, DependencyKind, FilesystemKind, Node, NodeFacts, NodeSpec,
    };
    use storage_engine::editing::EditError;
    let baseline = baseline()?;
    let request = requests(&baseline)?[1].clone();
    let source = request.referenced_nodes()[0];
    let mut session = PlanSession::new(baseline.clone())?;
    session.edit(session.revision(), PlanEdit::Add(request))?;
    let saved = session.draft_document();
    let frozen = session.freeze(session.revision())?;
    let json = frozen.document().canonical_json()?;
    let mut next = baseline.clone();
    next.id = uuid::Uuid::now_v7();
    let target = NodeId::new();
    let node = next
        .snapshot
        .graph
        .remove_node(&source)
        .ok_or("missing source")?;
    next.snapshot.graph.insert_new(target, node)?;
    let edges: Vec<_> = next
        .snapshot
        .graph
        .dependencies()
        .filter(|edge| edge.to == source)
        .cloned()
        .collect();
    for edge in edges {
        next.snapshot.graph.remove_dependency(&edge);
        let mut edge = edge;
        edge.to = target;
        next.snapshot.graph.insert_dependency(edge);
    }
    let review = session.prepare_rebase(session.revision(), next.clone())?;
    assert_eq!(review.preview().status(), PreviewStatus::Conditional);
    assert_eq!(session.draft_document(), saved);
    let consumer = NodeId::new();
    next.snapshot.graph.insert_new(
        consumer,
        Node {
            kind: NodeSpec {
                kind: NodeKind::Filesystem {
                    kind: FilesystemKind::Ext4,
                    label: None,
                },
                size: None,
            },
            size: NodeFacts::default(),
        },
    )?;
    next.snapshot.graph.insert_dependency(Dependency {
        from: target,
        to: consumer,
        kind: DependencyKind::Backs,
    });
    let review = session.prepare_rebase(session.revision(), next)?;
    assert_eq!(review.preview().status(), PreviewStatus::Invalid);
    assert_eq!(
        review.draft().operations[0].id,
        saved.draft.operations[0].id
    );
    assert!(
        matches!(review.draft().operations[0].request, PlanRequest::RemoveLv { lv } if lv == target)
    );
    session.accept_rebase(session.revision(), review)?;
    assert_eq!(session.undo(session.revision()), Err(EditError::NoUndo));
    assert!(session.freeze(session.revision()).is_err());
    assert_eq!(frozen.document().canonical_json()?, json);
    Ok(())
}

/// Read-only table metadata blocks deletion; unresolved mounts stay obligations and unsupported rebase fails.
#[test]
fn removal_keeps_readonly_mount_and_identity_checks() -> Result<(), Box<dyn Error>> {
    use storage_core::model::{BlockFacts, DeviceNumber, MountContext, MountSource, ObservedMount};
    use storage_engine::{
        CheckStatus, SimulationObligation,
        editing::{CorrelationError, RebaseError},
    };
    use storage_provider::ActionPrerequisite;
    let baseline = baseline()?;
    let selected = requests(&baseline)?;
    let table = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::PartitionTable(_)).then_some(*id))
        .ok_or("missing table")?;
    let mut readonly = baseline.clone();
    let mut node = readonly
        .snapshot
        .graph
        .node(&table)
        .ok_or("missing table")?
        .clone();
    node.size.block = Some(BlockFacts {
        paths: vec![],
        devno: DeviceNumber { major: 8, minor: 0 },
        geometry: None,
        read_only: Some(true),
    });
    readonly.snapshot.graph.replace_existing(table, node)?;
    let mut session = PlanSession::new(readonly)?;
    session.edit(session.revision(), PlanEdit::Add(selected[0].clone()))?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    assert!(
        session
            .preview_document()
            .diagnostics
            .iter()
            .any(
                |diagnostic| diagnostic.checks.iter().any(|check| check.prerequisite
                    == ActionPrerequisite::Writable(table)
                    && matches!(check.status, CheckStatus::Violated { .. }))
            )
    );

    let mut unresolved = baseline.clone();
    unresolved.snapshot.mounts.entries.push(ObservedMount {
        source: MountSource::Bind("/data".into()),
        target: "/alias".into(),
        options: vec![],
        context: MountContext::Host,
    });
    let mut session = PlanSession::new(unresolved)?;
    let mapping = selected[2].referenced_nodes()[0];
    session.edit(session.revision(), PlanEdit::Add(selected[2].clone()))?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    assert!(session.preview().calculated().ok_or("missing preview")?.report.unresolved_obligations.iter().any(|obligation| matches!(obligation, SimulationObligation::Prerequisite { prerequisite: ActionPrerequisite::NoUnexpectedConsumers(node), .. } if *node == mapping)));

    for request in [selected[0].clone(), selected[2].clone()] {
        let target = request.referenced_nodes()[0];
        let mut session = PlanSession::new(baseline.clone())?;
        session.edit(session.revision(), PlanEdit::Add(request))?;
        let saved = session.draft_document();
        let mut next = baseline.clone();
        next.id = uuid::Uuid::now_v7();
        assert!(
            matches!(session.prepare_rebase(session.revision(), next), Err(RebaseError::Correlation(CorrelationError::NoIdentity(node))) if node == target)
        );
        assert_eq!(session.draft_document(), saved);
    }
    Ok(())
}
