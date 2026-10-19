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

/// Imported descriptions are reanalysed and immutable; identical saved UUIDs never share facade handles.
#[test]
fn facade_plan_import_revalidates_data_and_owns_immutable_handles() -> Result<(), Box<dyn Error>> {
    let (_scratch, mut app, _, writes) = support::application()?;
    let session = app.create_session()?;
    let frozen = app.freeze(session, app.revision(session)?)?;
    let json = frozen.document().canonical_json()?;
    let first = app.import_plan_json(&json)?;
    let second = app.import_plan(frozen.document().clone())?;
    assert_ne!(first, second);
    assert_ne!(first.as_uuid(), frozen.document().plan_id);
    let mut modified = app.export_plan(first)?;
    modified.schema_version = u32::MAX;
    assert_eq!(
        app.import_plan(modified)
            .err()
            .ok_or("version must fail")?
            .code,
        ErrorCode::InvalidPlan
    );
    assert_eq!(app.export_plan(first)?, *frozen.document());
    let (_other_root, mut other, _, _) = support::application()?;
    other.import_plan_json(&json)?;
    assert_eq!(
        other.export_plan(first).err().ok_or("foreign plan")?.code,
        ErrorCode::UnknownPlan
    );
    let huge = " ".repeat(disksgraph::MAX_PLAN_JSON_BYTES + 1);
    assert_eq!(
        app.import_plan_json(&huge)
            .err()
            .ok_or("bounded parse")?
            .code,
        ErrorCode::ResourceLimit
    );
    for _ in 2..disksgraph::MAX_APPLICATION_OBJECTS {
        app.import_plan_json(&json)?;
    }
    assert_eq!(
        app.import_plan_json(&json).err().ok_or("plan limit")?.code,
        ErrorCode::ResourceLimit
    );
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// The facade obtains fresh final evidence, retains errors and never turns imported data into approval.
#[test]
fn facade_apply_retains_actual_runs_errors_and_source_map_boundaries() -> Result<(), Box<dyn Error>>
{
    use disksgraph::{
        PlanEdit, PlanRequest,
        model::{Bytes, LvmLvKind, NodeKind},
    };
    let (scratch, mut app, probes, writes) = support::application()?;
    let session = app.create_session()?;
    let empty = app.freeze(session, app.revision(session)?)?;
    let run = app.apply_frozen(&empty)?;
    let snapshot = app.run_snapshot(run)?;
    assert!(snapshot.execution.finished && snapshot.execution.verified);
    assert!(snapshot.execution.goal_verification.is_some());
    assert_eq!(probes.get(), 3);
    let page = app.events(run, 0)?;
    assert!(!page.gap && !page.events.is_empty());
    assert_eq!(app.events(run, page.next_sequence)?.events.len(), 0);
    assert_eq!(
        app.events(run, u64::MAX).err().ok_or("future cursor")?.code,
        ErrorCode::InvalidEventCursor
    );
    let baseline = app.export_draft(session)?.baseline;
    let vg = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::LvmVg { .. }).then_some(*id))
        .ok_or("VG")?;
    app.edit(
        session,
        app.revision(session)?,
        PlanEdit::Add(PlanRequest::CreateLv {
            vg,
            name: "data".into(),
            size: Bytes::new(16 * 1024 * 1024),
            layout: LvmLvKind::Linear,
            activate: true,
        }),
    )?;
    let frozen = app.freeze(session, app.revision(session)?)?;
    let error = app
        .apply_frozen(&frozen)
        .err()
        .ok_or("unknown native support must stop")?;
    assert_eq!(error.code, ErrorCode::PlanUnavailable);
    let failed = error
        .run_id
        .ok_or("failed attempt must remain inspectable")?;
    let snapshot = app.run_snapshot(failed)?;
    assert!(snapshot.execution.finished && !snapshot.execution.verified);
    assert_eq!(
        snapshot.error.as_ref().map(|error| error.code),
        Some(ErrorCode::PlanUnavailable)
    );
    assert_eq!(snapshot.source_map, frozen.source_map());
    assert!(!snapshot.source_map.is_empty());
    assert!(app.events(failed, 0)?.events.is_empty());
    let imported = app.import_plan(frozen.document().clone())?;
    let failed = app
        .apply_plan(imported)
        .err()
        .ok_or("import grants no approval")?
        .run_id
        .ok_or("retained imported attempt")?;
    assert!(app.run_snapshot(failed)?.source_map.is_empty());
    assert!(!scratch.0.join("active-attempt.json").exists());
    let (_other_root, other, _, _) = support::application()?;
    assert_eq!(
        other.run_snapshot(run).err().ok_or("foreign run")?.code,
        ErrorCode::UnknownRun
    );
    assert_eq!(
        other.events(run, 0).err().ok_or("foreign run events")?.code,
        ErrorCode::UnknownRun
    );
    std::fs::write(scratch.0.join("active-attempt.json"), b"corrupt incident")?;
    let failure = app
        .apply_frozen(&empty)
        .err()
        .ok_or("checkpoint blocks an empty plan too")?;
    assert_eq!(failure.code, ErrorCode::CheckpointBlocked);
    let failed = failure.run_id.ok_or("pre-dispatch failure retained")?;
    assert!(app.run_snapshot(failed)?.error.is_some());
    assert_eq!(
        std::fs::read(scratch.0.join("active-attempt.json"))?,
        b"corrupt incident"
    );
    assert_eq!(writes.get(), 0);
    Ok(())
}
