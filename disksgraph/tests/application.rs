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
