//! LUKS preparation uses ordinary session edits and never resolves or serializes secrets.

use std::error::Error;

use storage_core::model::{
    Bytes, FilesystemKind, LuksVersion, MountContext, MountEntry, MountSource, NodeGraph, NodeId,
    NodeKind,
};
use storage_engine::{
    HistoricalBaseline, PlanDocument, SimulationObligation,
    editing::{DraftDocument, EditError, PlanEdit, PlanRequest, PlanSession, PreviewStatus},
};
use storage_probe::ProbeSnapshot;
use storage_provider::{ActionPrerequisite, AllocationDomain, CredentialRef};

/// Creates an offline LV → LUKS → mapping → ext4 → mount stack from a synthetic probe record.
fn session_with_stack() -> Result<PlanSession, Box<dyn Error>> {
    let snapshot: ProbeSnapshot =
        serde_json::from_str(include_str!("../examples/fixtures/probe_snapshot.json"))?;
    let vg = snapshot.lvm_vgs[0].node;
    let baseline = HistoricalBaseline::from_lvm_observations(snapshot.current, &snapshot.lvm_vgs)?;
    let mut session = PlanSession::new(baseline)?;
    let credential = CredentialRef::from_uuid(uuid::Uuid::now_v7());
    let lv = session
        .add_lv(
            session.revision(),
            vg,
            "encrypted",
            Bytes::new(64 * 1024 * 1024),
        )?
        .created_node
        .ok_or("missing LV")?;
    let container = session
        .edit(
            session.revision(),
            PlanEdit::Add(PlanRequest::FormatLuks {
                backing: lv,
                version: LuksVersion::Luks2,
                credential,
            }),
        )?
        .created_node
        .ok_or("missing container")?;
    let mapping = session
        .edit(
            session.revision(),
            PlanEdit::Add(PlanRequest::OpenLuks {
                container,
                mapping_name: "crypt-data".to_owned(),
                credential,
                read_only: false,
            }),
        )?
        .created_node
        .ok_or("missing mapping")?;
    let filesystem = session
        .add_filesystem(session.revision(), mapping, FilesystemKind::Ext4)?
        .created_node
        .ok_or("missing filesystem")?;
    session.mount(
        session.revision(),
        MountEntry {
            source: MountSource::Filesystem(filesystem),
            target: "/data".into(),
            options: vec!["rw".to_owned()],
            context: MountContext::Host,
        },
    )?;
    Ok(session)
}

/// Frozen plans retain unknown credentials/approval and refuse weakened imported contracts.
#[test]
fn luks_stack_persists_freezes_and_remains_conditionally_authorized() -> Result<(), Box<dyn Error>>
{
    let mut session = session_with_stack()?;
    let baseline = session.baseline().clone();
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    let preview = session.preview().calculated().ok_or("missing preview")?;
    assert!(
        preview
            .request_coverage
            .iter()
            .all(|coverage| coverage.covered)
    );
    let credential_obligations = preview
        .report
        .unresolved_obligations
        .iter()
        .filter(|obligation| {
            matches!(
                obligation,
                SimulationObligation::Prerequisite {
                    prerequisite: ActionPrerequisite::CredentialAvailable(_),
                    ..
                }
            )
        })
        .count();
    assert_eq!(credential_obligations, 2);
    assert!(
        preview
            .report
            .unresolved_obligations
            .iter()
            .any(|obligation| matches!(
                obligation,
                SimulationObligation::Prerequisite {
                    prerequisite: ActionPrerequisite::DestructiveApproval(_),
                    ..
                }
            ))
    );
    for operation in session.draft().operations.iter().skip(1).take(3) {
        let node = preview
            .report
            .simulation
            .desired
            .graph
            .node(&operation.created_node.ok_or("missing output")?)
            .ok_or("missing desired node")?;
        assert_eq!(node.kind.size, None, "header overhead must not be guessed");
    }
    let saved = session.draft_document().to_json()?;
    let resumed = PlanSession::from_document(DraftDocument::from_json(&saved)?)?;
    assert_eq!(resumed.draft_document(), session.draft_document());
    assert_eq!(resumed.preview(), session.preview());
    assert!(!saved.contains("passphrase"));
    let frozen = session.freeze(session.revision())?;
    let json = frozen.document().canonical_json()?;
    let analysed = PlanDocument::from_json(&json)?.analyse_against(&baseline)?;
    assert_eq!(
        analysed.report.simulation.desired,
        preview.report.simulation.desired
    );
    let mut weakened = frozen.document().clone();
    for action in &mut weakened.actions {
        action
            .informational_contract
            .prerequisites
            .retain(|prerequisite| {
                !matches!(prerequisite, ActionPrerequisite::CredentialAvailable(_))
            });
    }
    assert!(weakened.analyse_against(&baseline).is_err());

    let format = session.draft().operations[1].id;
    let open = session.draft().operations[2].id;
    let format_request = session.draft().operations[1].request.clone();
    let open_request = session.draft().operations[2].request.clone();
    for invalid in 0..4 {
        let mut request = if invalid == 3 {
            format_request.clone()
        } else {
            open_request.clone()
        };
        let operation = if invalid == 3 { format } else { open };
        match &mut request {
            PlanRequest::OpenLuks {
                mapping_name,
                read_only,
                ..
            } => match invalid {
                0 => *read_only = true,
                1 => *mapping_name = "../invalid".to_owned(),
                _ => *mapping_name = "".to_owned(),
            },
            PlanRequest::FormatLuks { credential, .. } => {
                *credential = CredentialRef::from_uuid(uuid::Uuid::nil())
            }
            _ => return Err("unexpected recipe".into()),
        }
        session.edit(session.revision(), PlanEdit::Replace { operation, request })?;
        assert_eq!(session.preview().status(), PreviewStatus::Invalid);
        assert!(session.freeze(session.revision()).is_err());
        let expected = if invalid == 0 {
            session.draft().operations[3].id
        } else {
            operation
        };
        assert!(
            session
                .preview_document()
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.operation == Some(expected))
        );
        let document = session.draft_document();
        let resumed = PlanSession::from_document(DraftDocument::from_json(&document.to_json()?)?)?;
        assert_eq!(resumed.draft_document(), document);
        assert_eq!(resumed.preview(), session.preview());
        session.undo(session.revision())?;
        assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    }

    // Previously planned consumers and names are checked against the shared replay prefix.
    for request in [format_request, open_request] {
        let receipt = session.edit(session.revision(), PlanEdit::Add(request))?;
        assert_eq!(session.preview().status(), PreviewStatus::Invalid);
        assert!(
            session
                .preview_document()
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.operation == receipt.added_operation)
        );
        session.undo(session.revision())?;
    }
    session.edit(
        session.revision(),
        PlanEdit::MoveBefore {
            operation: open,
            before: Some(format),
        },
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    session.undo(session.revision())?;
    let impact = session.removal_impact(format)?;
    assert_eq!(impact.len(), 4);
    assert!(matches!(
        session.edit(
            session.revision(),
            PlanEdit::Remove {
                operations: vec![format]
            }
        ),
        Err(EditError::DependentOperations { .. })
    ));
    session.edit(session.revision(), PlanEdit::Remove { operations: impact })?;
    assert_eq!(
        session.draft().operations.len(),
        1,
        "LV remains independently requested"
    );
    session.undo(session.revision())?;
    session.redo(session.revision())?;
    assert_eq!(session.baseline(), &baseline);
    assert_eq!(frozen.document().canonical_json()?, json);
    Ok(())
}

/// Rebase remaps an existing VG input while preserving planned crypto IDs and credential references.
#[test]
fn planned_luks_stack_rebases_through_its_supported_vg_identity() -> Result<(), Box<dyn Error>> {
    let mut session = session_with_stack()?;
    let saved = session.draft_document();
    let mut next = session.baseline().clone();
    let AllocationDomain::LvmVolumeGroup(source) = next.capacity_evidence[0].domain;
    let node = next
        .snapshot
        .graph
        .node(&source)
        .ok_or("missing VG")?
        .clone();
    let target = NodeId::new();
    next.id = uuid::Uuid::now_v7();
    next.snapshot.graph = NodeGraph::new();
    next.snapshot.graph.insert_new(target, node)?;
    next.capacity_evidence[0].domain = AllocationDomain::LvmVolumeGroup(target);
    let review = session.prepare_rebase(session.revision(), next)?;
    assert_eq!(review.preview().status(), PreviewStatus::Conditional);
    assert_eq!(review.correlations().len(), 1);
    for (old, new) in saved
        .draft
        .operations
        .iter()
        .zip(&review.draft().operations)
    {
        assert_eq!(old.id, new.id);
        assert_eq!(old.created_node, new.created_node);
    }
    assert_eq!(review.draft().operations[1..], saved.draft.operations[1..]);
    session.accept_rebase(session.revision(), review)?;
    assert!(
        matches!(session.draft().operations[0].request, PlanRequest::CreateLv { vg, .. } if vg == target)
    );
    assert_eq!(session.undo(session.revision()), Err(EditError::NoUndo));
    Ok(())
}

/// Observed read-only containers cannot produce a writable mapping or accept filesystem creation.
#[test]
fn existing_read_only_container_keeps_its_write_boundary() -> Result<(), Box<dyn Error>> {
    use storage_core::model::{BlockFacts, DeviceNumber, Node, NodeFacts, NodeSpec, Presence};
    let mut baseline: HistoricalBaseline =
        serde_json::from_str(include_str!("../examples/fixtures/editing_baseline.json"))?;
    let container = NodeId::new();
    baseline.capacity_evidence.clear();
    baseline.snapshot.graph = NodeGraph::new();
    baseline.snapshot.graph.insert_new(
        container,
        Node {
            kind: NodeSpec {
                kind: NodeKind::LuksContainer {
                    version: LuksVersion::Luks2,
                },
                size: Some(Bytes::new(32 * 1024 * 1024)),
            },
            size: NodeFacts {
                presence: Presence::Present,
                block: Some(BlockFacts {
                    paths: vec!["/dev/example-container".into()],
                    devno: DeviceNumber {
                        major: 253,
                        minor: 0,
                    },
                    geometry: None,
                    read_only: Some(true),
                }),
                ..NodeFacts::default()
            },
        },
    )?;
    let mut session = PlanSession::new(baseline.clone())?;
    let receipt = session.edit(
        session.revision(),
        PlanEdit::Add(PlanRequest::OpenLuks {
            container,
            mapping_name: "crypt-read-only".to_owned(),
            credential: CredentialRef::from_uuid(uuid::Uuid::now_v7()),
            read_only: false,
        }),
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    assert!(
        session
            .preview_document()
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic
                .checks
                .iter()
                .any(|check| check.prerequisite == ActionPrerequisite::Writable(container)))
    );
    let mut request = session.draft().operations[0].request.clone();
    if let PlanRequest::OpenLuks { read_only, .. } = &mut request {
        *read_only = true;
    }
    session.edit(
        session.revision(),
        PlanEdit::Replace {
            operation: receipt.added_operation.ok_or("missing operation")?,
            request,
        },
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    session.add_filesystem(
        session.revision(),
        receipt.created_node.ok_or("missing mapping")?,
        FilesystemKind::Ext4,
    )?;
    assert_eq!(session.preview().status(), PreviewStatus::Invalid);
    assert!(session.freeze(session.revision()).is_err());
    assert_eq!(session.baseline(), &baseline);
    Ok(())
}
