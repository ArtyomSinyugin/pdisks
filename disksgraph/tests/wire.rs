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

/// Prepared JSON cannot replace the private candidate, foreign/stale handles cannot rebase a session.
#[test]
fn rebase_reviews_are_owned_single_use_and_revision_checked() -> Result<(), Box<dyn Error>> {
    let (_scratch, app, probes, writes) = support::application()?;
    let mut protocol = Protocol::new(app, true);
    let session = call(&mut protocol, "session_create", json!({}))?["result"]["session"].clone();
    let original =
        call(&mut protocol, "session_export", json!({"session":session}))?["result"].clone();
    let reviewed = call(
        &mut protocol,
        "session_prepare_rebase",
        json!({"session":session,"revision":0}),
    )?;
    let review = reviewed["result"]["review"].clone();
    assert!(review.is_string(), "{reviewed}");
    assert_eq!(
        call(&mut protocol, "session_export", json!({"session":session}))?["result"],
        original
    );
    assert!(reviewed["result"]["baseline"]["snapshot"].is_object());
    assert!(reviewed["result"]["draft"].is_object());
    assert!(reviewed["result"]["correlations"].is_array());
    let other =
        call(&mut protocol, "session_import", json!({"draft":original}))?["result"]["session"]
            .clone();
    assert_eq!(
        call(
            &mut protocol,
            "session_accept_rebase",
            json!({"session":other,"revision":0,"review":review})
        )?["error"]["code"],
        "review_mismatch"
    );
    assert_eq!(
        call(
            &mut protocol,
            "session_accept_rebase",
            json!({"session":session,"revision":1,"review":review})
        )?["error"]["code"],
        "review_mismatch"
    );
    assert_eq!(
        call(
            &mut protocol,
            "session_accept_rebase",
            json!({"session":session,"revision":0,"review":review,"candidate":reviewed["result"]})
        )?["error"]["code"],
        "invalid_request"
    );
    let (_foreign_root, app, _, _) = support::application()?;
    let mut foreign = Protocol::new(app, true);
    assert_eq!(
        call(&mut foreign, "review_close", json!({"review":review}))?["error"]["code"],
        "unknown_review"
    );
    assert_eq!(
        call(
            &mut protocol,
            "session_accept_rebase",
            json!({"session":session,"revision":0,"review":review})
        )?["result"]["revision"],
        1
    );
    assert_eq!(
        call(&mut protocol, "session_export", json!({"session":session}))?["result"]["baseline"],
        reviewed["result"]["baseline"]
    );
    assert_eq!(
        call(&mut protocol, "review_close", json!({"review":review}))?["error"]["code"],
        "unknown_review"
    );
    let stale = call(
        &mut protocol,
        "session_prepare_rebase",
        json!({"session":session,"revision":1}),
    )?["result"]["review"]
        .clone();
    assert_eq!(
        call(
            &mut protocol,
            "session_edit",
            json!({"session":session,"revision":1,"edit":disksgraph::PlanEdit::Clear}),
        )?["result"]["revision"],
        2
    );
    let rejected = call(
        &mut protocol,
        "session_accept_rebase",
        json!({"session":session,"revision":1,"review":stale}),
    )?;
    assert_eq!(
        rejected["error"]["detail"]["code"], "revision_conflict",
        "{rejected}"
    );
    assert_eq!(
        call(&mut protocol, "review_close", json!({"review":stale}))?["error"]["code"],
        "unknown_review"
    );
    let before = probes.get();
    assert_eq!(
        call(
            &mut protocol,
            "session_prepare_rebase",
            json!({"session":session,"revision":1})
        )?["error"]["detail"]["code"],
        "revision_conflict"
    );
    assert_eq!(probes.get(), before);
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Review capacity is checked before probe; discard and session close release only retained memory.
#[test]
fn review_capacity_rejects_before_probe_and_session_close_releases_it() -> Result<(), Box<dyn Error>>
{
    let (_scratch, app, probes, writes) = support::application()?;
    let mut protocol = Protocol::new(app, true);
    let session = call(&mut protocol, "session_create", json!({}))?["result"]["session"].clone();
    let mut first = Value::Null;
    for _ in 0..disksgraph::MAX_APPLICATION_OBJECTS {
        first = call(
            &mut protocol,
            "session_prepare_rebase",
            json!({"session":session,"revision":0}),
        )?["result"]["review"]
            .clone();
        assert!(first.is_string());
    }
    let before = probes.get();
    assert_eq!(
        call(
            &mut protocol,
            "session_prepare_rebase",
            json!({"session":session,"revision":0})
        )?["error"]["code"],
        "resource_limit"
    );
    assert_eq!(probes.get(), before);
    assert!(
        call(&mut protocol, "review_close", json!({"review":first}))?
            .get("error")
            .is_none()
    );
    let replacement = call(
        &mut protocol,
        "session_prepare_rebase",
        json!({"session":session,"revision":0}),
    )?["result"]["review"]
        .clone();
    assert_ne!(replacement, first);
    call(&mut protocol, "session_close", json!({"session":session}))?;
    assert_eq!(
        call(&mut protocol, "review_close", json!({"review":replacement}))?["error"]["code"],
        "unknown_review"
    );
    let session = call(&mut protocol, "session_create", json!({}))?["result"]["session"].clone();
    assert!(
        call(
            &mut protocol,
            "session_prepare_rebase",
            json!({"session":session,"revision":0})
        )?["result"]["review"]
            .is_string()
    );
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Invalid execution roots fail before even opening a native manifest.
#[test]
fn native_configuration_checks_root_before_manifest() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let scratch = support::Scratch::new()?;
    let missing = scratch.0.join("missing.json");
    std::fs::set_permissions(&scratch.0, std::fs::Permissions::from_mode(0o755))?;
    assert!(matches!(
        disksgraph::native::open(&scratch.0, Some(&missing)),
        Err(disksgraph::native::OpenError::Application(_))
    ));
    std::fs::set_permissions(&scratch.0, std::fs::Permissions::from_mode(0o700))?;
    let alias = scratch.0.join("root-link");
    symlink(&scratch.0, &alias)?;
    assert!(matches!(
        disksgraph::native::open(&alias, Some(&missing)),
        Err(disksgraph::native::OpenError::Application(_))
    ));
    assert!(matches!(
        disksgraph::native::open(&scratch.0, Some(&missing)),
        Err(disksgraph::native::OpenError::Configuration(_))
    ));
    assert!(!scratch.0.join("execution.lock").exists());
    Ok(())
}
