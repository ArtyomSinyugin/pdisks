//! Application protocol parity and boundary rejection without native writes.

mod support;
use disksgraph::{DraftDocument, SessionId, wire::Protocol};
use serde_json::{Value, json};
use std::error::Error;

/// Sends one versioned request and decodes its unambiguous result/error envelope.
fn call(
    protocol: &mut Protocol<
        disksgraph::host::host::LinuxLiveHost<'static, support::FixturePolicy, support::NoApproval>,
        support::NoWrites,
    >,
    method: &str,
    params: Value,
) -> Result<Value, serde_json::Error> {
    serde_json::from_str(&protocol.call_json(
        &json!({"protocol_version":1, "request":{"method":method,"params":params}}).to_string(),
    ))
}

/// Unsupported versions and unknown fields are rejected before observation or state mutation.
#[test]
fn protocol_rejects_version_size_and_unknown_fields_before_probe() -> Result<(), Box<dyn Error>> {
    let (_scratch, app, probes, writes) = support::application()?;
    let mut protocol = Protocol::new(app, true);
    for (input, code) in [
        (
            json!({"protocol_version":99,"request":{"method":"future_method"}}).to_string(),
            "unsupported_version",
        ),
        (
            json!({"protocol_version":1,"request":{"method":"probe","params":{"approval":true}}})
                .to_string(),
            "invalid_request",
        ),
        (
            json!({"protocol_version":1,"request":{"method":"probe","params":{}},"extra":true})
                .to_string(),
            "invalid_request",
        ),
        (
            " ".repeat(disksgraph::wire::MAX_REQUEST_BYTES + 1),
            "resource_limit",
        ),
    ] {
        let reply: Value = serde_json::from_str(&protocol.call_json(&input))?;
        assert_eq!(reply["error"]["code"], code);
    }
    assert_eq!(probes.get(), 0);
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Saved inputs, preview, freeze and final evidence use the same facade methods and ownership checks.
#[test]
fn protocol_preserves_facade_semantics_and_process_local_handles() -> Result<(), Box<dyn Error>> {
    let (_scratch, mut direct, _, _) = support::application()?;
    let session = direct.create_session()?;
    let draft: DraftDocument = direct.export_draft(session)?;
    let (_wire_root, app, _, writes) = support::application()?;
    let mut protocol = Protocol::new(app, true);
    let imported = call(&mut protocol, "session_import", json!({"draft":draft}))?;
    let id: SessionId = serde_json::from_value(imported["result"]["session"].clone())?;
    assert_ne!(session, id);
    let preview = call(&mut protocol, "session_preview", json!({"session":id}))?;
    assert_eq!(
        preview["result"],
        serde_json::to_value(direct.preview_document(session)?)?
    );
    let foreign = call(&mut protocol, "session_preview", json!({"session":session}))?;
    assert_eq!(foreign["error"]["detail"]["code"], "unknown_session");
    let frozen = call(
        &mut protocol,
        "session_freeze",
        json!({"session":id,"revision":0}),
    )?;
    let plan = call(
        &mut protocol,
        "plan_import",
        json!({"document":frozen["result"]}),
    )?;
    let outcome = call(
        &mut protocol,
        "apply_blocking",
        json!({"plan":plan["result"]["plan"]}),
    )?;
    assert_eq!(outcome["result"]["verified"], true);
    let run = outcome["result"]["run_id"].clone();
    assert_eq!(
        call(&mut protocol, "run_snapshot", json!({"run":run}))?["result"],
        outcome["result"]
    );
    assert!(
        call(&mut protocol, "run_events", json!({"run":run,"since":0}))?["result"]["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
    );
    call(&mut protocol, "session_close", json!({"session":id}))?;
    assert_eq!(
        call(&mut protocol, "session_preview", json!({"session":id}))?["error"]["detail"]["code"],
        "unknown_session"
    );
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Offline mode must never present an empty provider list as fresh native state.
#[test]
fn saved_input_context_rejects_fresh_operations() -> Result<(), Box<dyn Error>> {
    let (_scratch, app, probes, _) = support::application()?;
    let mut protocol = Protocol::new(app, false);
    assert_eq!(
        call(&mut protocol, "session_create", json!({}))?["error"]["code"],
        "live_unavailable"
    );
    assert_eq!(
        call(&mut protocol, "probe", json!({}))?["error"]["code"],
        "live_unavailable"
    );
    assert_eq!(probes.get(), 0);
    Ok(())
}

/// An owned saved-input application reports missing native reads rather than inventing device absence.
#[test]
fn owned_native_configuration_preserves_explicit_missing_evidence() -> Result<(), Box<dyn Error>> {
    let (scratch, _, _, _) = support::application()?;
    let mut app = disksgraph::native::open(&scratch.0, None)?;
    let first = app.probe()?;
    let second = app.probe()?;
    assert!(second.reference.epoch > first.reference.epoch);
    assert!(second.snapshot.diagnostics.iter().any(|item| {
        item.code == "provider.probe_failed"
            && item
                .evidence
                .as_ref()
                .is_some_and(|evidence| evidence.contains("saved inputs only"))
    }));
    assert!(
        disksgraph::native::open(&scratch.0, Some(&scratch.0.join("missing-manifest"))).is_err()
    );
    Ok(())
}

/// Job vocabulary is strict; a synchronous adapter cannot silently execute an asynchronous request.
#[test]
fn job_requests_require_an_owner_and_observed_apply_preserves_final_evidence()
-> Result<(), Box<dyn Error>> {
    let (_scratch, mut app, probes, writes) = support::application()?;
    let session = app.create_session()?;
    let frozen = app.freeze(session, app.revision(session)?)?;
    let plan = app.import_plan(frozen.document().clone())?;
    let mut protocol = Protocol::new(app, true);
    let before = probes.get();
    for (method, params) in [
        ("apply_start", json!({"plan":plan})),
        ("job_poll", json!({"job":1,"since":0})),
        ("job_close", json!({"job":1})),
        ("job_cancel", json!({"job":1})),
    ] {
        assert_eq!(
            call(&mut protocol, method, params)?["error"]["code"],
            "unsupported_operation"
        );
    }
    assert_eq!(probes.get(), before);
    assert_eq!(
        call(
            &mut protocol,
            "job_poll",
            json!({"job":1,"since":0,"extra":true})
        )?["error"]["code"],
        "invalid_request"
    );
    let mut updates = Vec::new();
    let terminal =
        protocol.apply_plan_with_updates(plan, &mut |snapshot, _| updates.push(snapshot))?;
    assert!(updates.iter().any(|snapshot| !snapshot.execution.finished));
    assert_eq!(serde_json::to_value(updates.last())?, terminal);
    let run = terminal["run_id"].clone();
    assert_eq!(
        call(&mut protocol, "run_snapshot", json!({"run":run}))?["result"],
        terminal
    );
    assert_eq!(writes.get(), 0);
    Ok(())
}
