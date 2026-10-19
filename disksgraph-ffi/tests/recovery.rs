//! The ABI's shared owner/job transport retains crash evidence with an explicit synthetic host factory.

#[path = "../../disksgraph/tests/support/mod.rs"]
mod support;
use disksgraph_ffi::transport;
use serde_json::{json, Value};
use std::{
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    sync::mpsc,
    time::{Duration, Instant},
};

/// Uses the normal versioned command/reply path, including the worker-owned protocol.
fn method(context: u64, method: &str, params: Value) -> Result<Value, Box<dyn Error>> {
    let reply = transport::call(
        context,
        &json!({"protocol_version":1,"request":{"method":method,"params":params}}).to_string(),
    )
    .map_err(|error| format!("transport: {error:?}"))?;
    Ok(serde_json::from_str(&reply)?)
}

/// Background apply, retained polling, mailbox close and owner join never acknowledge an old attempt.
#[test]
fn job_transport_preserves_coordinator_checkpoint_across_owners() -> Result<(), Box<dyn Error>> {
    let (scratch, mut original, _, _) = support::application()?;
    let session = original.create_session()?;
    let plan = original
        .freeze(session, original.revision(session)?)?
        .document()
        .clone();
    let bytes =
        include_bytes!("../../storage-runtime/tests/fixtures/coordinator-active-attempt.json");
    disksgraph::host::checkpoint::ActiveAttempt::from_bytes(bytes)?;
    let active = scratch.0.join("active-attempt.json");
    fs::write(&active, bytes)?;
    fs::set_permissions(&active, fs::Permissions::from_mode(0o600))?;
    drop(original);
    for _ in 0..2 {
        let (after_close, counts) = mpsc::sync_channel(1);
        let opened = transport::open_with(
            &json!({"abi_version":1,"protocol_version":1,"root":scratch.0,"manifest":"synthetic-fixture.json"}).to_string(),
            move |root, _, worker| {
                // The Rust factory supplies the fixture; no native manifest or permission is inferred.
                let (app, probes, writes) = support::open(&root).map_err(|error| transport::Error::OpenFailed(error.to_string()))?;
                worker.serve(app);
                let _ = after_close.send((probes.get(), writes.get()));
                Ok(())
            },
        ).map_err(|error| format!("open: {error:?}"))?;
        let context = opened["context"].as_u64().ok_or("context")?;
        let imported = method(context, "plan_import", json!({"document":plan}))?;
        assert!(imported["result"]["plan"].is_string(), "{imported}");
        let started = method(
            context,
            "apply_start",
            json!({"plan":imported["result"]["plan"]}),
        )?;
        let job = started["result"]["job"]
            .as_u64()
            .ok_or_else(|| format!("job: {started}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            let polled = method(context, "job_poll", json!({"job":job,"since":0}))?;
            if polled["result"]["finished"] == true {
                break polled["result"].clone();
            }
            assert!(Instant::now() < deadline, "job did not finish: {polled}");
            std::thread::yield_now();
        };
        assert_eq!(result["snapshot"]["finished"], true);
        assert_eq!(result["snapshot"]["verified"], false);
        assert_eq!(result["snapshot"]["error"]["code"], "checkpoint_blocked");
        assert_eq!(result["snapshot"]["event_sequence"], 0);
        assert_eq!(result["events"]["events"], json!([]));
        assert_eq!(result["snapshot"]["source_map"], json!([]));
        assert!(method(context, "job_close", json!({"job":job}))?["error"].is_null());
        transport::close(context).map_err(|error| format!("close: {error:?}"))?;
        assert_eq!(counts.recv_timeout(Duration::from_secs(5))?, (0, 0));
        assert_eq!(fs::read(&active)?, bytes);
    }
    Ok(())
}
