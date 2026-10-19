//! Recovery through the real command parser/adapter with an explicit synthetic host, no native writes.

#[path = "../../disksgraph/tests/support/mod.rs"]
mod support;
use std::{error::Error, ffi::OsString, fs};

/// Retained refusal is exit 3 and context teardown preserves the actual coordinator's marker.
#[test]
fn command_apply_preserves_coordinator_checkpoint_after_reopen() -> Result<(), Box<dyn Error>> {
    let (scratch, mut original, _, _) = support::application()?;
    let session = original.create_session()?;
    let plan = original.freeze(session, original.revision(session)?)?;
    let file = scratch.0.join("plan.json");
    dg_cli::files::save_document(&file, &plan.document().canonical_json()?)?;
    let bytes =
        include_bytes!("../../storage-runtime/tests/fixtures/coordinator-active-attempt.json");
    // This artifact is valid schema data captured after SIGKILL, rather than an arbitrary corrupt file.
    disksgraph::host::checkpoint::ActiveAttempt::from_bytes(bytes)?;
    let active = scratch.0.join("active-attempt.json");
    dg_cli::files::save_document(&active, std::str::from_utf8(bytes)?)?;
    drop(original);
    for _ in 0..2 {
        // The manifest word belongs to parser acceptance only; this trusted fixture provides its host.
        let invocation = dg_cli::Invocation::parse([
            OsString::from("plan"),
            "apply".into(),
            "--root".into(),
            scratch.0.as_os_str().to_owned(),
            "--file".into(),
            file.as_os_str().to_owned(),
            "--manifest".into(),
            "synthetic-fixture.json".into(),
            "--events".into(),
            "jsonl".into(),
        ])?;
        let (mut app, probes, writes) = support::open(&invocation.root)?;
        let mut events = Vec::new();
        let result = dg_cli::execute(&mut app, &invocation.command, &mut events)?;
        assert_eq!(dg_cli::result_exit_code(&result), 3);
        assert_eq!(result["run"]["finished"], true);
        assert_eq!(result["run"]["verified"], false);
        assert_eq!(result["run"]["event_sequence"], 0);
        assert_eq!(result["run"]["error"]["code"], "checkpoint_blocked");
        assert_eq!(result["run"]["source_map"], serde_json::json!([]));
        assert_eq!(String::from_utf8(events)?.lines().count(), 1);
        assert_eq!(probes.get(), 0);
        assert_eq!(writes.get(), 0);
        drop(app);
        assert_eq!(fs::read(&active)?, bytes);
    }
    Ok(())
}
