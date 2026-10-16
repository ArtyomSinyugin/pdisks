//! Existing ext4/LV resize requests retain stable storage identities and explicit preparation.

use std::error::Error;
use storage_core::model::{Bytes, NodeId, NodeKind};
use storage_engine::{
    HistoricalBaseline,
    editing::{EditError, PlanEdit, PlanRequest, PlanSession, PreviewStatus},
};

/// Binary mebibyte for exact synthetic capacities.
const MIB: u64 = 1024 * 1024;

/// Loads the mounted 128 MiB ext4/LV fixture with 16 free extents of 4 MiB.
fn baseline() -> Result<HistoricalBaseline, Box<dyn Error>> {
    Ok(serde_json::from_str(include_str!(
        "../examples/fixtures/resize_baseline.json"
    ))?)
}

/// Selects the existing LV and filesystem without relying on device paths or names.
fn nodes(baseline: &HistoricalBaseline) -> Result<(NodeId, NodeId), Box<dyn Error>> {
    let lv = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::LvmLv(_)).then_some(*id))
        .ok_or("missing LV")?;
    let filesystem = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::Filesystem { .. }).then_some(*id))
        .ok_or("missing filesystem")?;
    Ok((lv, filesystem))
}

/// Resize edits update only their own target size, preserve history, and reject stale/wrong commands.
#[test]
fn editing_resize_size_keeps_existing_identities() -> Result<(), Box<dyn Error>> {
    let baseline = baseline()?;
    let (lv, filesystem) = nodes(&baseline)?;
    let mut session = PlanSession::new(baseline.clone())?;
    let resize_lv = session
        .edit(
            session.revision(),
            PlanEdit::Add(PlanRequest::ResizeLv {
                lv,
                new_size: Bytes::new(160 * MIB),
            }),
        )?
        .added_operation
        .ok_or("missing operation")?;
    let resize_fs = session
        .edit(
            session.revision(),
            PlanEdit::Add(PlanRequest::ResizeFilesystem {
                filesystem,
                new_size: Bytes::new(160 * MIB),
            }),
        )?
        .added_operation
        .ok_or("missing operation")?;
    assert_ne!(session.preview().status(), PreviewStatus::Invalid);
    let before = session.draft_document();
    session.set_resize_size(session.revision(), resize_lv, Bytes::new(164 * MIB))?;
    session.set_resize_size(session.revision(), resize_fs, Bytes::new(164 * MIB))?;
    for (old, new) in before
        .draft
        .operations
        .iter()
        .zip(&session.draft().operations)
    {
        assert_eq!(old.id, new.id);
        assert_eq!(old.created_node, None);
        assert_eq!(new.created_node, None);
        assert_eq!(
            old.request.referenced_nodes(),
            new.request.referenced_nodes()
        );
    }
    let resized = session.draft_document();
    session.undo(session.revision())?;
    session.redo(session.revision())?;
    assert_eq!(session.draft(), &resized.draft);
    let stale = before.revision;
    assert!(matches!(
        session.set_resize_size(stale, resize_lv, Bytes::new(0)),
        Err(EditError::RevisionConflict { .. })
    ));
    assert_eq!(
        session.set_created_lv_size(session.revision(), resize_lv, Bytes::new(1)),
        Err(EditError::NotCreatedLv(resize_lv))
    );
    let label = session
        .edit(
            session.revision(),
            PlanEdit::Add(PlanRequest::SetFilesystemLabel {
                filesystem,
                label: None,
            }),
        )?
        .added_operation
        .ok_or("missing label operation")?;
    let unchanged = session.draft_document();
    assert_eq!(
        session.set_resize_size(session.revision(), label, Bytes::new(1)),
        Err(EditError::NotResize(label))
    );
    assert_eq!(session.draft_document(), unchanged);
    assert_eq!(session.baseline(), &baseline);
    Ok(())
}

/// Explicit grow/shrink requests share prefix state and allocation; frozen history stays immutable.
#[test]
fn existing_stack_grows_then_shrinks_with_explicit_preparation() -> Result<(), Box<dyn Error>> {
    use storage_core::model::{MountContext, MountEntry, MountSource, OperationIntent};
    use storage_engine::{PlanDocument, SimulationObligation, editing::DraftDocument};
    use storage_provider::{ActionPrerequisite, AllocationDomain};
    let baseline = baseline()?;
    let (lv, filesystem) = nodes(&baseline)?;
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
    let frozen = session.freeze(session.revision())?;
    let frozen_json = frozen.document().canonical_json()?;
    PlanDocument::from_json(&frozen_json)?.analyse_against(&baseline)?;
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
        PlanRequest::ResizeLv {
            lv,
            new_size: Bytes::new(96 * MIB),
        },
        PlanRequest::Mount(MountEntry {
            source: MountSource::Filesystem(filesystem),
            target: "/data".into(),
            options: vec!["rw".to_owned()],
            context: MountContext::Host,
        }),
    ] {
        session.edit(session.revision(), PlanEdit::Add(request))?;
    }
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    let preview = session
        .preview()
        .calculated()
        .ok_or("missing shrink preview")?;
    assert!(
        preview
            .request_coverage
            .iter()
            .all(|coverage| coverage.covered)
    );
    assert!(preview.report.unresolved_obligations.iter().any(|obligation| matches!(obligation,
        SimulationObligation::Prerequisite { prerequisite: ActionPrerequisite::FilesystemMinimumSize { filesystem: target, .. }, .. } if *target == filesystem)));
    for node in [lv, filesystem] {
        assert_eq!(
            preview
                .report
                .simulation
                .desired
                .graph
                .node(&node)
                .ok_or("missing resized node")?
                .kind
                .size,
            Some(Bytes::new(96 * MIB))
        );
    }
    assert_eq!(preview.report.simulation.desired.mounts.runtime.len(), 1);
    let saved = session.draft_document().to_json()?;
    let resumed = PlanSession::from_document(DraftDocument::from_json(&saved)?)?;
    assert_eq!(resumed.draft_document(), session.draft_document());
    assert_eq!(resumed.preview(), session.preview());
    let AllocationDomain::LvmVolumeGroup(vg) = baseline.capacity_evidence[0].domain;
    // Baseline free 64 MiB + net shrink 32 MiB = 96 MiB, with no double release.
    session.add_lv(session.revision(), vg, "reuse", Bytes::new(96 * MIB))?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    let excess = session
        .add_lv(session.revision(), vg, "excess", Bytes::new(4 * MIB))?
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
    session.redo(session.revision())?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    session.undo(session.revision())?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    assert_eq!(session.baseline(), &baseline);
    assert_eq!(frozen.document().canonical_json()?, frozen_json);
    Ok(())
}

/// Mounted shrink, wrong order, stale checks, bad geometry, and exhausted VG capacity stay invalid.
#[test]
fn unsafe_resize_requests_remain_editable_but_cannot_freeze() -> Result<(), Box<dyn Error>> {
    use storage_core::model::{MountContext, OperationIntent};
    use storage_engine::editing::DraftDocument;
    let baseline = baseline()?;
    let (lv, filesystem) = nodes(&baseline)?;
    let unmount = PlanRequest::Unmount {
        target: "/data".into(),
        context: MountContext::Host,
    };
    let check = PlanRequest::Maintenance(OperationIntent::Check {
        target: filesystem,
        force: true,
    });
    let shrink = PlanRequest::ResizeFilesystem {
        filesystem,
        new_size: Bytes::new(96 * MIB),
    };
    let scenarios = vec![
        vec![PlanRequest::ResizeLv {
            lv,
            new_size: Bytes::new(96 * MIB),
        }],
        vec![shrink.clone()],
        vec![unmount.clone(), shrink.clone()],
        vec![
            unmount.clone(),
            PlanRequest::Maintenance(OperationIntent::Check {
                target: filesystem,
                force: false,
            }),
            shrink.clone(),
        ],
        vec![PlanRequest::ResizeFilesystem {
            filesystem,
            new_size: Bytes::new(160 * MIB),
        }],
        vec![PlanRequest::ResizeLv {
            lv,
            new_size: Bytes::new(196 * MIB),
        }],
        vec![PlanRequest::ResizeLv {
            lv,
            new_size: Bytes::new(161 * MIB),
        }],
        vec![PlanRequest::ResizeLv {
            lv,
            new_size: Bytes::new(0),
        }],
        vec![PlanRequest::ResizeFilesystem {
            filesystem,
            new_size: Bytes::new(0),
        }],
        vec![
            unmount,
            check,
            PlanRequest::ResizeFilesystem {
                filesystem,
                new_size: Bytes::new(112 * MIB),
            },
            shrink,
        ],
    ];
    for (index, requests) in scenarios.into_iter().enumerate() {
        let mut session = PlanSession::new(baseline.clone())?;
        let mut last = None;
        for request in requests {
            last = session
                .edit(session.revision(), PlanEdit::Add(request))?
                .added_operation;
        }
        assert_eq!(
            session.preview().status(),
            PreviewStatus::Invalid,
            "scenario {index}"
        );
        assert!(session.freeze(session.revision()).is_err());
        assert!(
            session
                .preview_document()
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.operation == last),
            "scenario {index}"
        );
        let json = session.draft_document().to_json()?;
        let resumed = PlanSession::from_document(DraftDocument::from_json(&json)?)?;
        assert_eq!(resumed.draft(), session.draft());
        assert_eq!(resumed.preview(), session.preview());
        assert_eq!(session.baseline(), &baseline);
    }
    // A known read-only endpoint cannot be overridden by a filesystem resize request.
    let mut readonly = baseline.clone();
    let mut node = readonly
        .snapshot
        .graph
        .node(&lv)
        .ok_or("missing LV")?
        .clone();
    node.size
        .block
        .as_mut()
        .ok_or("missing block facts")?
        .read_only = Some(true);
    readonly.snapshot.graph.replace_existing(lv, node)?;
    let mut session = PlanSession::new(readonly)?;
    session.edit(
        session.revision(),
        PlanEdit::Add(PlanRequest::ResizeFilesystem {
            filesystem,
            new_size: Bytes::new(128 * MIB),
        }),
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    Ok(())
}

/// Existing resize references correlate by UUID; changed free capacity invalidates the new preview.
#[test]
fn resize_rebase_rechecks_capacity_and_preserves_frozen_history() -> Result<(), Box<dyn Error>> {
    use std::collections::HashMap;
    use storage_core::model::{MountSource, NodeGraph};
    use storage_provider::AllocationDomain;
    let baseline = baseline()?;
    let (lv, filesystem) = nodes(&baseline)?;
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
    let saved = session.draft_document();
    let frozen = session.freeze(session.revision())?;
    let json = frozen.document().canonical_json()?;
    let mapping: HashMap<_, _> = baseline
        .snapshot
        .graph
        .nodes()
        .map(|(id, _)| (*id, NodeId::new()))
        .collect();
    let mut next = baseline.clone();
    next.id = uuid::Uuid::now_v7();
    next.snapshot.graph = NodeGraph::new();
    for (id, node) in baseline.snapshot.graph.nodes() {
        next.snapshot.graph.insert_new(mapping[id], node.clone())?;
    }
    for edge in baseline.snapshot.graph.dependencies() {
        let mut edge = edge.clone();
        edge.from = mapping[&edge.from];
        edge.to = mapping[&edge.to];
        next.snapshot.graph.insert_dependency(edge);
    }
    next.snapshot.mounts.entries[0].source = MountSource::Filesystem(mapping[&filesystem]);
    let AllocationDomain::LvmVolumeGroup(vg) = next.capacity_evidence[0].domain;
    next.capacity_evidence[0].domain = AllocationDomain::LvmVolumeGroup(mapping[&vg]);
    next.capacity_evidence[0].available = storage_provider::AllocationQuantity::new(0);
    let review = session.prepare_rebase(session.revision(), next)?;
    assert_eq!(review.preview().status(), PreviewStatus::Invalid);
    assert_eq!(review.correlations().len(), 2);
    assert_eq!(session.draft_document(), saved);
    assert!(
        matches!(review.draft().operations[0].request, PlanRequest::ResizeLv { lv: actual, .. } if actual == mapping[&lv])
    );
    assert!(
        matches!(review.draft().operations[1].request, PlanRequest::ResizeFilesystem { filesystem: actual, .. } if actual == mapping[&filesystem])
    );
    for (old, new) in saved
        .draft
        .operations
        .iter()
        .zip(&review.draft().operations)
    {
        assert_eq!(old.id, new.id);
        assert_eq!(new.created_node, None);
    }
    session.accept_rebase(session.revision(), review)?;
    for operation in &saved.draft.operations {
        session.set_resize_size(session.revision(), operation.id, Bytes::new(128 * MIB))?;
    }
    assert_ne!(session.preview().status(), PreviewStatus::Invalid);
    assert_eq!(frozen.document().canonical_json()?, json);
    Ok(())
}
