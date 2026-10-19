//! Public application boundary tests; no disk device or native mutator is used.

mod support;
use disksgraph::{
    ErrorCode,
    host::{ProviderAction, capability::OperationAvailability},
};
use std::error::Error;

/// Fresh advisory state does not authorize execution and corrupt markers remain blocking errors.
#[test]
fn facade_probes_and_reports_capabilities_without_dispatch() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, probes, writes) = support::application()?;
    let first = app.probe()?;
    let second = app.probe()?;
    assert_eq!(probes.get(), 2);
    assert!(second.reference.epoch > first.reference.epoch);
    let operation = ProviderAction::Lvm(storage_provider::LvmAction::SetActivation {
        target: disksgraph::model::NodeId::new(),
        active: true,
    });
    let report = app.capabilities(&[operation])?;
    assert_eq!(probes.get(), 3);
    assert!(!report.incident_pending);
    assert!(matches!(
        report.operations[0].availability,
        OperationAvailability::Unknown(_)
    ));
    assert!(serde_json::to_string(&report)?.contains("unknown"));
    std::fs::write(scratch.0.join("active-attempt.json"), b"corrupt marker")?;
    assert_eq!(
        app.capabilities(&[])
            .err()
            .ok_or("corrupt marker must fail")?
            .code,
        ErrorCode::CheckpointBlocked
    );
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Machine codes remain distinct for cursor errors and event exhaustion without parsing diagnostics.
#[test]
fn application_error_codes_are_serializable_and_typed() -> Result<(), Box<dyn Error>> {
    let error =
        disksgraph::DisksGraphError::from(storage_engine::ExecutionRunError::InvalidEventCursor {
            requested: 2,
            latest: 1,
        });
    assert_eq!(error.code, ErrorCode::InvalidEventCursor);
    assert_eq!(
        serde_json::to_value(&error)?["code"],
        "invalid_event_cursor"
    );
    let error = disksgraph::DisksGraphError::from(storage_runtime::host::HostError::ActiveAttempt);
    assert_eq!(error.code, ErrorCode::CheckpointBlocked);
    Ok(())
}

/// Sessions own same-pass baselines, preserve editing history and reject stale/foreign handles.
#[test]
fn facade_sessions_keep_revision_history_and_import_ownership() -> Result<(), Box<dyn Error>> {
    use disksgraph::{
        PlanEdit, PlanRequest,
        model::{Bytes, LvmLvKind, NodeKind},
    };
    let (_scratch, mut app, probes, writes) = support::application()?;
    let session = app.create_session()?;
    assert_eq!(probes.get(), 1);
    let original = app.export_draft(session)?;
    let vg = original
        .baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::LvmVg { .. }).then_some(*id))
        .ok_or("VG")?;
    let revision = app.revision(session)?;
    let receipt = app.edit(
        session,
        revision,
        PlanEdit::Add(PlanRequest::CreateLv {
            vg,
            name: "data".into(),
            size: Bytes::new(16 * 1024 * 1024),
            layout: LvmLvKind::Linear,
            activate: true,
        }),
    )?;
    assert!(receipt.created_node.is_some());
    assert_eq!(
        app.edit(session, revision, PlanEdit::Clear)
            .err()
            .ok_or("stale revision")?
            .code,
        ErrorCode::RevisionConflict
    );
    let undone = app.undo(session, receipt.revision)?;
    assert!(app.export_draft(session)?.draft.operations.is_empty());
    let redone = app.redo(session, undone)?;
    let exported = app.export_draft(session)?;
    assert_eq!(
        exported.draft.operations[0].created_node,
        receipt.created_node
    );
    let imported = app.import_draft(exported.clone())?;
    assert_ne!(session, imported);
    assert_eq!(app.revision(imported)?, redone);
    assert_eq!(
        app.undo(imported, redone)
            .err()
            .ok_or("saved history must not be restored")?
            .code,
        ErrorCode::EditRejected
    );
    let (_other_root, mut other, _, _) = support::application()?;
    other.import_draft(exported)?;
    assert_eq!(
        other.revision(session).err().ok_or("foreign session")?.code,
        ErrorCode::UnknownSession
    );
    let candidate = app.prepare_rebase(session, redone)?;
    assert_eq!(probes.get(), 2);
    app.accept_rebase(session, redone, candidate)?;
    app.edit(
        session,
        app.revision(session)?,
        PlanEdit::Add(PlanRequest::CreateLv {
            vg,
            name: "too-large".into(),
            size: Bytes::new(128 * 1024 * 1024 * 1024),
            layout: LvmLvKind::Linear,
            activate: false,
        }),
    )?;
    assert_eq!(
        app.preview(session)?.status(),
        disksgraph::PreviewStatus::Invalid
    );
    assert!(app.preview_document(session)?.desired.is_none());
    let invalid = app.export_draft(session)?;
    let reopened = app.import_draft(invalid)?;
    assert_eq!(
        app.preview(reopened)?.status(),
        disksgraph::PreviewStatus::Invalid
    );
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Collection limits stop before another provider call and never evict live objects implicitly.
#[test]
fn facade_bounds_sessions_before_probing() -> Result<(), Box<dyn Error>> {
    let (_scratch, mut app, probes, writes) = support::application()?;
    for _ in 0..disksgraph::MAX_APPLICATION_OBJECTS {
        app.create_session()?;
    }
    assert_eq!(
        app.create_session().err().ok_or("session limit")?.code,
        ErrorCode::ResourceLimit
    );
    assert_eq!(probes.get(), disksgraph::MAX_APPLICATION_OBJECTS);
    assert_eq!(writes.get(), 0);
    Ok(())
}
