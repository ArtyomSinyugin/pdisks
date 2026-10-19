//! Incident transport tests use recorded observations and private marker files, never native writes.

mod support;
use disksgraph::{
    IncidentAcknowledgmentPolicy, IncidentInspection, LiveCheckStatus,
    host::{
        BackendId, LvmAction, ProviderAction,
        checkpoint::{ActiveAttempt, CHECKPOINT_SCHEMA_VERSION, TargetFingerprint},
    },
    model::{Bytes, ExternalId, LvmLvKind, NodeId},
    wire::{Protocol, encode_reply, parse_request},
};
use serde_json::{Value, json};
use std::{error::Error, fs, os::unix::fs::PermissionsExt};

/// Concrete recorded host, independent of any permissive native configuration.
type Wire = Protocol<
    disksgraph::host::host::LinuxLiveHost<'static, support::FixturePolicy, support::NoApproval>,
    support::NoWrites,
>;

/// Explicit test-only decision; no native operation or outside writer exists in this fixture.
struct Policy {
    allow: bool,
    calls: usize,
}
impl IncidentAcknowledgmentPolicy for Policy {
    /// Counts independent safety evaluation, which must follow exact review confirmation and fresh checks.
    fn check(&mut self, _: &IncidentInspection, _: &IncidentInspection) -> LiveCheckStatus {
        self.calls += 1;
        if self.allow {
            LiveCheckStatus::Satisfied
        } else {
            LiveCheckStatus::Unknown {
                required_evidence: "fixture quiescence absent".into(),
            }
        }
    }
}

/// Exercises the strict JSON boundary with trusted Rust-only policy injection, never a JSON policy flag.
fn call(
    wire: &mut Wire,
    method: &str,
    params: Value,
    policy: &mut impl IncidentAcknowledgmentPolicy,
) -> Result<Value, serde_json::Error> {
    let input =
        json!({"protocol_version":1,"request":{"method":method,"params":params}}).to_string();
    serde_json::from_str(&encode_reply(
        parse_request(&input).and_then(|request| wire.call_with_policy(request, policy)),
    ))
}

/// Creates a complete private synthetic marker without calling a dispatcher or checkpoint arm path.
fn marker(
    app: &mut support::Application,
    root: &std::path::Path,
) -> Result<ActiveAttempt, Box<dyn Error>> {
    let session = app.create_session()?;
    let draft = app.export_draft(session)?;
    let vg = *draft.baseline.snapshot.graph.nodes().next().ok_or("VG")?.0;
    let marker = ActiveAttempt {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        run_id: uuid::Uuid::now_v7(),
        plan_id: uuid::Uuid::now_v7(),
        action_id: disksgraph::ActionId::new(0),
        operation_id: None,
        action: ProviderAction::Lvm(LvmAction::CreateLv {
            vg,
            planned_node_id: NodeId::new(),
            name: "data".into(),
            size: Bytes::new(16 * 1024 * 1024),
            layout: LvmLvKind::Linear,
        }),
        targets: vec![TargetFingerprint {
            planned_id: vg,
            external_ids: vec![ExternalId::LvmVgUuid("example-vg-uuid".into())],
        }],
        plan_digest: "a".repeat(64),
        backend_id: BackendId::new("application-no-writes").ok_or("backend")?,
    };
    let path = root.join("active-attempt.json");
    fs::write(&path, marker.to_bytes()?)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(marker)
}

/// Exact confirmation is necessary but cannot supply the default owner's missing independent safety policy.
#[test]
fn incident_reviews_are_owned_and_default_acknowledgment_fails_closed() -> Result<(), Box<dyn Error>>
{
    let (scratch, mut app, probes, writes) = support::application()?;
    let attempt = marker(&mut app, &scratch.0)?;
    let bytes = fs::read(scratch.0.join("active-attempt.json"))?;
    let mut wire = Protocol::new(app, true);
    let mut policy = Policy {
        allow: true,
        calls: 0,
    };
    let shown = call(&mut wire, "incident_review", json!({}), &mut policy)?;
    let review = shown["result"]["review"].clone();
    let token = shown["result"]["token"].clone();
    assert_eq!(
        shown["result"]["incident"]["attempt"],
        serde_json::to_value(&attempt)?
    );
    let before = probes.get();
    let confirmation = json!({"review":review,"run_id":attempt.run_id,"token":token});
    for field in ["review", "run_id", "token"] {
        let mut wrong = confirmation.clone();
        wrong[field] = json!(uuid::Uuid::now_v7());
        let reply = call(&mut wire, "incident_acknowledge", wrong, &mut policy)?;
        assert_eq!(
            reply["error"]["code"],
            if field == "review" {
                "unknown_review"
            } else {
                "review_mismatch"
            }
        );
    }
    let mut forged = confirmation.clone();
    forged["policy"] = json!({"allow":true});
    assert_eq!(
        call(&mut wire, "incident_acknowledge", forged, &mut policy)?["error"]["code"],
        "invalid_request"
    );
    let (_root, foreign, _, _) = support::application()?;
    let mut foreign = Protocol::new(foreign, true);
    assert_eq!(
        call(
            &mut foreign,
            "incident_acknowledge",
            confirmation.clone(),
            &mut policy
        )?["error"]["code"],
        "unknown_review"
    );
    assert_eq!(
        call(
            &mut wire,
            "session_accept_rebase",
            json!({"session":attempt.run_id,"revision":0,"review":review}),
            &mut policy
        )?["error"]["code"],
        "unknown_review"
    );
    assert_eq!(probes.get(), before);
    assert_eq!(policy.calls, 0);
    let input = json!({"protocol_version":1,"request":{"method":"incident_acknowledge","params":confirmation}}).to_string();
    let refused: Value = serde_json::from_str(&wire.call_json(&input))?;
    assert_eq!(
        refused["error"]["detail"]["code"], "incident_rejected",
        "{refused}"
    );
    assert_eq!(fs::read(scratch.0.join("active-attempt.json"))?, bytes);
    assert_eq!(
        call(&mut wire, "incident_acknowledge", confirmation, &mut policy)?["error"]["code"],
        "unknown_review"
    );
    assert_eq!(policy.calls, 0);
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// A real confirmed attempt consumes its review even on changed-marker or independent policy refusal.
#[test]
fn incident_acknowledgment_preserves_runtime_change_and_safety_gates() -> Result<(), Box<dyn Error>>
{
    let (scratch, mut app, _, writes) = support::application()?;
    let mut attempt = marker(&mut app, &scratch.0)?;
    let mut wire = Protocol::new(app, true);
    let mut policy = Policy {
        allow: true,
        calls: 0,
    };
    let shown = call(&mut wire, "incident_review", json!({}), &mut policy)?["result"].clone();
    attempt.plan_digest = "b".repeat(64);
    fs::write(scratch.0.join("active-attempt.json"), attempt.to_bytes()?)?;
    let confirm = json!({"review":shown["review"],"run_id":attempt.run_id,"token":shown["token"]});
    assert_eq!(
        call(
            &mut wire,
            "incident_acknowledge",
            confirm.clone(),
            &mut policy
        )?["error"]["detail"]["code"],
        "incident_rejected"
    );
    assert_eq!(policy.calls, 0);
    assert_eq!(
        fs::read(scratch.0.join("active-attempt.json"))?,
        attempt.to_bytes()?
    );
    assert_eq!(
        call(&mut wire, "incident_acknowledge", confirm, &mut policy)?["error"]["code"],
        "unknown_review"
    );
    let shown = call(&mut wire, "incident_review", json!({}), &mut policy)?["result"].clone();
    policy.allow = false;
    assert_eq!(
        call(
            &mut wire,
            "incident_acknowledge",
            json!({"review":shown["review"],"run_id":attempt.run_id,"token":shown["token"]}),
            &mut policy
        )?["error"]["detail"]["code"],
        "incident_rejected"
    );
    assert_eq!(policy.calls, 1);
    assert!(scratch.0.join("active-attempt.json").exists());
    let shown = call(&mut wire, "incident_review", json!({}), &mut policy)?["result"].clone();
    policy.allow = true;
    let cleared = call(
        &mut wire,
        "incident_acknowledge",
        json!({"review":shown["review"],"run_id":attempt.run_id,"token":shown["token"]}),
        &mut policy,
    )?;
    assert_eq!(cleared["result"]["acknowledged"], true, "{cleared}");
    assert!(!scratch.0.join("active-attempt.json").exists());
    assert_eq!(policy.calls, 2);
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Empty review, explicit discard and dropping the owner never acknowledge a checkpoint implicitly.
#[test]
fn review_discard_and_owner_drop_preserve_checkpoint() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, probes, writes) = support::application()?;
    let mut policy = Policy {
        allow: true,
        calls: 0,
    };
    let (_empty_root, empty_app, empty_probes, _) = support::application()?;
    let mut empty = Protocol::new(empty_app, true);
    assert_eq!(
        call(&mut empty, "incident_review", json!({}), &mut policy)?["result"],
        json!({"review":null,"incident":null})
    );
    assert_eq!(empty_probes.get(), 0);
    let attempt = marker(&mut app, &scratch.0)?;
    let mut wire = Protocol::new(app, true);
    let shown = call(&mut wire, "incident_review", json!({}), &mut policy)?["result"].clone();
    let before = probes.get();
    call(
        &mut wire,
        "review_close",
        json!({"review":shown["review"]}),
        &mut policy,
    )?;
    assert_eq!(probes.get(), before);
    assert_eq!(policy.calls, 0);
    assert_eq!(
        fs::read(scratch.0.join("active-attempt.json"))?,
        attempt.to_bytes()?
    );
    call(&mut wire, "incident_review", json!({}), &mut policy)?;
    drop(wire);
    assert_eq!(
        fs::read(scratch.0.join("active-attempt.json"))?,
        attempt.to_bytes()?
    );
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Rebase and incident reviews share one bound; session cleanup never consumes an incident review.
#[test]
fn mixed_review_capacity_and_saved_only_guards_precede_fresh_work() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, probes, writes) = support::application()?;
    let attempt = marker(&mut app, &scratch.0)?;
    let mut wire = Protocol::new(app, true);
    let mut policy = Policy {
        allow: true,
        calls: 0,
    };
    let incident = call(&mut wire, "incident_review", json!({}), &mut policy)?["result"].clone();
    let session =
        call(&mut wire, "session_create", json!({}), &mut policy)?["result"]["session"].clone();
    let mut rebase = Value::Null;
    for _ in 1..disksgraph::MAX_APPLICATION_OBJECTS {
        rebase = call(
            &mut wire,
            "session_prepare_rebase",
            json!({"session":session,"revision":0}),
            &mut policy,
        )?["result"]["review"]
            .clone();
        assert!(rebase.is_string());
    }
    let before = probes.get();
    assert_eq!(
        call(&mut wire, "incident_review", json!({}), &mut policy)?["error"]["code"],
        "resource_limit"
    );
    assert_eq!(
        call(
            &mut wire,
            "session_prepare_rebase",
            json!({"session":session,"revision":0}),
            &mut policy
        )?["error"]["code"],
        "resource_limit"
    );
    assert_eq!(probes.get(), before);
    assert_eq!(
        call(
            &mut wire,
            "incident_acknowledge",
            json!({"review":rebase,"run_id":attempt.run_id,"token":incident["token"]}),
            &mut policy
        )?["error"]["code"],
        "unknown_review"
    );
    call(
        &mut wire,
        "session_close",
        json!({"session":session}),
        &mut policy,
    )?;
    call(
        &mut wire,
        "review_close",
        json!({"review":incident["review"]}),
        &mut policy,
    )?;
    assert!(
        call(&mut wire, "incident_review", json!({}), &mut policy)?["result"]["review"].is_string()
    );
    assert_eq!(policy.calls, 0);
    assert_eq!(
        fs::read(scratch.0.join("active-attempt.json"))?,
        attempt.to_bytes()?
    );
    let (_offline_root, offline_app, offline_probes, _) = support::application()?;
    let mut offline = Protocol::new(offline_app, false);
    for (method, params) in [
        (
            "session_prepare_rebase",
            json!({"session":session,"revision":0}),
        ),
        ("incident_review", json!({})),
        (
            "incident_acknowledge",
            json!({"review":incident["review"],"run_id":attempt.run_id,"token":incident["token"]}),
        ),
    ] {
        assert_eq!(
            call(&mut offline, method, params, &mut policy)?["error"]["code"],
            "live_unavailable"
        );
    }
    assert_eq!(offline_probes.get(), 0);
    assert_eq!(writes.get(), 0);
    Ok(())
}
