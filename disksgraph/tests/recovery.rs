//! Public recovery gates over a checkpoint captured from the real synthetic coordinator.

mod support;
use disksgraph::{ErrorCode, RunId, host::checkpoint::ActiveAttempt};
use std::{error::Error, fs, os::unix::fs::PermissionsExt};

/// Reopening never reconstructs a dead run or clears its evidence while rejecting a new apply.
#[test]
fn reopened_facade_preserves_coordinator_checkpoint_and_blocks_apply() -> Result<(), Box<dyn Error>>
{
    let (scratch, mut original, _, _) = support::application()?;
    let session = original.create_session()?;
    let plan = original
        .freeze(session, original.revision(session)?)?
        .document()
        .clone();
    let bytes =
        include_bytes!("../../storage-runtime/tests/fixtures/coordinator-active-attempt.json");
    let attempt = ActiveAttempt::from_bytes(bytes)?;
    let dead: RunId = serde_json::from_value(serde_json::json!(attempt.run_id))?;
    let path = scratch.0.join("active-attempt.json");
    fs::write(&path, bytes)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    drop(original);
    for _ in 0..2 {
        let (mut app, probes, writes) = support::open(&scratch.0)?;
        assert_eq!(
            app.run_snapshot(dead)
                .err()
                .ok_or("dead run must not be restored")?
                .code,
            ErrorCode::UnknownRun
        );
        let failure = app
            .apply_blocking(plan.clone())
            .err()
            .ok_or("pending checkpoint")?;
        assert_eq!(failure.code, ErrorCode::CheckpointBlocked);
        let run = failure.run_id.ok_or("retained refusal")?;
        assert_ne!(run, dead);
        let snapshot = app.run_snapshot(run)?;
        assert!(snapshot.execution.finished);
        assert!(!snapshot.execution.verified);
        assert_eq!(snapshot.execution.event_sequence, 0);
        assert!(snapshot.source_map.is_empty());
        assert_eq!(
            snapshot.error.ok_or("retained error")?.code,
            ErrorCode::CheckpointBlocked
        );
        assert!(app.events(run, 0)?.events.is_empty());
        assert_eq!(probes.get(), 0);
        assert_eq!(writes.get(), 0);
        app.close_run(run)?;
        drop(app);
        assert_eq!(fs::read(&path)?, bytes);
    }
    Ok(())
}
