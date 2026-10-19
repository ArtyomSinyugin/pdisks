#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]

//! Separately linked consumer executable, loading the real cdylib exclusively through Stabby reports.

#[path = "../../packaging/consumer/src/api.rs"]
mod dylib;

use dylib::{Api, Error};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

/// Private directory removed only after both native/ABI application owners have dropped.
struct Scratch(PathBuf);
impl Scratch {
    /// Allocates a fresh existing private root, never a host storage device.
    fn new() -> Result<Self, Error> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "disksgraph-consumer-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
    /// Saved-input mode is the portable default; native configuration is an explicit ignored test.
    fn config(&self) -> Value {
        json!({"abi_version":1,"protocol_version":1,"root":self.0})
    }
}
impl Drop for Scratch {
    /// Removes only files belonging to this synthetic consumer root.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Cargo builds the cdylib as a normal library target beside this separate integration executable.
fn library() -> Result<PathBuf, Error> {
    Ok(std::env::current_exe()?
        .parent()
        .ok_or("test executable directory")?
        .join("libdisksgraph_ffi.so"))
}

/// Builds a historical draft from the existing recorded observation, never a native provider adapter.
fn draft() -> Result<Value, Error> {
    let recorded: Value = serde_json::from_str(include_str!(
        "../../storage-engine/examples/fixtures/probe_snapshot.json"
    ))?;
    Ok(json!({
        "schema_version":5,"session_id":"00000000-0000-7000-8000-000000000005","revision":0,
        "baseline":{"id":"00000000-0000-7000-8000-000000000006","snapshot":recorded["current"],"capacity_evidence":[]},
        "draft":{"operations":[],"protected_scope":{"nodes":[],"runtime_mounts":[],"persistent_mounts":[]}},
    }))
}

/// ABI mismatch, JSON versioning, instance ownership and editing round-trips are checked in a real .so.
#[test]
fn checked_import_preserves_editing_parity_and_rejects_foreign_handles() -> Result<(), Error> {
    let scratch = Scratch::new()?;
    let api = Api::load(&library()?, "disksgraph")?;
    assert_eq!(api.version()?, 1);
    assert!(api.rejects_wrong_signature());
    // Guest-only host exports belong exclusively to the separately built acceptance artifact.
    assert!(Api::load(&library()?, "disksgraph_vm")?.version().is_err());
    assert_eq!(
        api.open(&json!({"abi_version":99}))?["error"]["code"],
        "unsupported_abi_version"
    );
    assert_eq!(
        api.open(&json!({"abi_version":1,"protocol_version":99}))?["error"]["code"],
        "unsupported_version"
    );
    let first = api.open(&scratch.config())?["result"]["context"]
        .as_u64()
        .ok_or("first context")?;
    let other = api.open(&scratch.config())?["result"]["context"]
        .as_u64()
        .ok_or("other context")?;
    assert_ne!(first, other);
    assert_eq!(
        api.call(
            first,
            &json!({"protocol_version":99,"request":{"method":"future"}})
        )?["error"]["code"],
        "unsupported_version"
    );
    let imported = api.method(first, "session_import", json!({"draft":draft()?}))?;
    let session = imported["result"]["session"].clone();
    assert!(session.is_string(), "{imported}");
    assert_eq!(
        api.method(other, "session_preview", json!({"session":session}))?["error"]["detail"]
            ["code"],
        "unknown_session"
    );
    let edit = json!({"session":session,"revision":0,"edit":{"add":{"create_lv":{
        "vg":"00000000-0000-7000-8000-000000000002","name":"consumer-data","size":16777216,"layout":"Linear","activate":true,
    }}}});
    assert_eq!(
        api.method(first, "session_edit", edit.clone())?["result"]["revision"],
        1
    );
    assert_eq!(
        api.method(first, "session_edit", edit)?["error"]["detail"]["code"],
        "revision_conflict"
    );
    assert_eq!(
        api.method(
            first,
            "session_undo",
            json!({"session":session,"revision":1})
        )?["result"]["revision"],
        2
    );
    assert_eq!(
        api.method(
            first,
            "session_redo",
            json!({"session":session,"revision":2})
        )?["result"]["revision"],
        3
    );
    let saved = api.method(first, "session_export", json!({"session":session}))?["result"].clone();
    let mut direct = disksgraph::native::open(&scratch.0, None)?;
    let direct_id =
        direct.import_draft(disksgraph::DraftDocument::from_json(&saved.to_string())?)?;
    assert_eq!(
        api.method(first, "session_preview", json!({"session":session}))?["result"],
        serde_json::to_value(direct.preview_document(direct_id)?)?
    );
    let frozen = api.method(
        first,
        "session_freeze",
        json!({"session":session,"revision":3}),
    )?["result"]
        .clone();
    let actual = disksgraph::PlanDocument::from_json(&frozen.to_string())?;
    let mut expected = direct
        .freeze(direct_id, direct.revision(direct_id)?)?
        .document()
        .clone();
    expected.plan_id = actual.plan_id;
    assert_eq!(expected.canonical_json()?, actual.canonical_json()?);
    let plan =
        api.method(first, "plan_import", json!({"document":frozen}))?["result"]["plan"].clone();
    assert!(plan.is_string());
    assert_eq!(
        api.method(first, "apply_blocking", json!({"plan":plan}))?["error"]["code"],
        "live_unavailable"
    );
    assert_eq!(
        api.method(first, "apply_start", json!({"plan":plan}))?["error"]["code"],
        "live_unavailable"
    );
    assert_eq!(
        api.method(first, "job_poll", json!({"job":1,"since":0}))?["error"]["code"],
        "unknown_job"
    );
    assert_eq!(
        api.method(first, "probe", json!({}))?["error"]["code"],
        "live_unavailable"
    );
    for (method, params) in [
        (
            "session_prepare_rebase",
            json!({"session":session,"revision":3}),
        ),
        ("incident_review", json!({})),
        (
            "incident_acknowledge",
            json!({"review":session,"run_id":session,"token":session}),
        ),
    ] {
        assert_eq!(
            api.method(first, method, params)?["error"]["code"],
            "live_unavailable"
        );
    }
    assert_eq!(
        api.method(first, "review_close", json!({"review":session}))?["error"]["code"],
        "unknown_review"
    );
    fs::write(scratch.0.join("active-attempt.json"), b"retained incident")?;
    api.close(other)?;
    api.close(first)?;
    assert_eq!(
        api.method(first, "session_preview", json!({"session":session}))?["error"]["code"],
        "unknown_context"
    );
    assert_eq!(
        fs::read(scratch.0.join("active-attempt.json"))?,
        b"retained incident"
    );
    Ok(())
}

/// Native libraries/permissions vary by host; run explicitly with DISKSGRAPH_NATIVE_MANIFEST set.
#[test]
#[ignore = "requires explicitly configured installed native read libraries"]
fn native_provider_calls_retain_one_owner_and_failed_run_evidence() -> Result<(), Error> {
    let manifest = std::env::var_os("DISKSGRAPH_NATIVE_MANIFEST")
        .ok_or("DISKSGRAPH_NATIVE_MANIFEST is required")?;
    let scratch = Scratch::new()?;
    let api = Api::load(&library()?, "disksgraph")?;
    let mut config = scratch.config();
    config["manifest"] = json!(PathBuf::from(manifest));
    let opened = api.open(&config)?;
    let context = opened["result"]["context"]
        .as_u64()
        .ok_or_else(|| format!("native open failed: {opened}"))?;
    let first = api.method(context, "probe", json!({}))?;
    let second = api.method(context, "probe", json!({}))?;
    assert!(first.get("error").is_none(), "{first}");
    assert_eq!(
        first["result"]["reference"]["probe_instance"],
        second["result"]["reference"]["probe_instance"]
    );
    assert!(
        second["result"]["reference"]["epoch"].as_u64()
            > first["result"]["reference"]["epoch"].as_u64()
    );
    let session = api.method(context, "session_import", json!({"draft":draft()?}))?["result"]
        ["session"]
        .clone();
    let document = api.method(
        context,
        "session_freeze",
        json!({"session":session,"revision":0}),
    )?["result"]
        .clone();
    let plan =
        api.method(context, "plan_import", json!({"document":document}))?["result"]["plan"].clone();
    let outcome = api.method(context, "apply_blocking", json!({"plan":plan}))?;
    assert_eq!(outcome["result"]["finished"], true, "{outcome}");
    assert_eq!(outcome["result"]["verified"], false);
    let run = outcome["result"]["run_id"].clone();
    assert_eq!(
        api.method(context, "run_snapshot", json!({"run":run}))?["result"],
        outcome["result"]
    );
    assert!(
        api.method(context, "run_events", json!({"run":run,"since":0}))?["result"]["events"]
            .is_array()
    );
    assert!(!scratch.0.join("active-attempt.json").exists());
    let job = api.method(context, "apply_start", json!({"plan":plan}))?["result"]["job"].clone();
    assert!(job.is_u64());
    let other = api.open(&scratch.config())?["result"]["context"]
        .as_u64()
        .ok_or("other context")?;
    assert_eq!(
        api.method(other, "job_poll", json!({"job":job,"since":0}))?["error"]["code"],
        "unknown_job"
    );
    let terminal = poll_job(&api, context, &job)?;
    assert_eq!(terminal["error"], Value::Null, "{terminal}");
    assert_eq!(terminal["snapshot"]["finished"], true);
    assert_eq!(terminal["snapshot"]["verified"], false);
    let async_run = terminal["snapshot"]["run_id"].clone();
    assert!(async_run.is_string());
    assert_ne!(async_run, run);
    assert_eq!(
        api.method(context, "run_snapshot", json!({"run":async_run}))?["result"],
        terminal["snapshot"]
    );
    assert_eq!(
        api.method(context, "run_events", json!({"run":async_run,"since":0}))?["result"],
        terminal["events"]
    );
    assert_eq!(
        api.method(context, "apply_start", json!({"plan":plan}))?["error"]["code"],
        "resource_limit"
    );
    let future = terminal["events"]["next_sequence"]
        .as_u64()
        .ok_or("event cursor")?
        + 1;
    assert_eq!(
        api.method(context, "job_poll", json!({"job":job,"since":future}))?["error"]["code"],
        "invalid_event_cursor"
    );
    assert_eq!(
        api.method(context, "job_cancel", json!({"job":job}))?["error"]["code"],
        "unsupported_operation"
    );
    assert!(api
        .method(context, "job_close", json!({"job":job}))?
        .get("error")
        .is_none());
    assert_eq!(
        api.method(context, "job_poll", json!({"job":job,"since":0}))?["error"]["code"],
        "unknown_job"
    );
    assert_eq!(
        api.method(context, "run_snapshot", json!({"run":async_run}))?["result"],
        terminal["snapshot"]
    );
    let invalid = json!("00000000-0000-7000-8000-000000000099");
    let next =
        api.method(context, "apply_start", json!({"plan":invalid}))?["result"]["job"].clone();
    assert_ne!(next, job);
    let rejected = poll_job(&api, context, &next)?;
    assert_eq!(rejected["snapshot"], Value::Null);
    assert_eq!(rejected["error"]["code"], "application");
    assert_eq!(rejected["error"]["detail"]["code"], "unknown_plan");
    assert!(!scratch.0.join("active-attempt.json").exists());
    api.close(other)?;
    api.close(context)?;
    Ok(())
}

/// Polls a real imported job with a deadline; queued/in-flight snapshots are never mistaken for success.
fn poll_job(api: &Api, context: u64, job: &Value) -> Result<Value, Error> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut since = 0;
    loop {
        let response = api.method(context, "job_poll", json!({"job":job,"since":since}))?;
        assert!(response.get("error").is_none(), "{response}");
        let result = response["result"].clone();
        if result["finished"] == true {
            // Fetch a complete retained page to compare with the ordinary run_events method.
            return Ok(
                api.method(context, "job_poll", json!({"job":job,"since":0}))?["result"].clone(),
            );
        }
        since = result["events"]["next_sequence"]
            .as_u64()
            .ok_or("job cursor")?;
        assert!(
            std::time::Instant::now() < deadline,
            "job did not finish: {result}"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Real native reads prepare a private rebase candidate; incident writes remain independently refused.
#[test]
#[ignore = "requires explicitly configured installed native read libraries"]
fn native_reviews_are_owned_and_acknowledgment_preserves_marker() -> Result<(), Error> {
    let manifest = std::env::var_os("DISKSGRAPH_NATIVE_MANIFEST")
        .ok_or("DISKSGRAPH_NATIVE_MANIFEST is required")?;
    let scratch = Scratch::new()?;
    let api = Api::load(&library()?, "disksgraph")?;
    let mut config = scratch.config();
    config["manifest"] = json!(PathBuf::from(manifest));
    let opened = api.open(&config)?;
    let context = opened["result"]["context"]
        .as_u64()
        .ok_or_else(|| format!("native open failed: {opened}"))?;
    let other = api.open(&config)?["result"]["context"]
        .as_u64()
        .ok_or("other context")?;
    let session = api.method(context, "session_import", json!({"draft":draft()?}))?["result"]
        ["session"]
        .clone();
    let original =
        api.method(context, "session_export", json!({"session":session}))?["result"].clone();
    let prepared = api.method(
        context,
        "session_prepare_rebase",
        json!({"session":session,"revision":0}),
    )?;
    let candidate = &prepared["result"];
    assert!(candidate["review"].is_string(), "{prepared}");
    assert!(candidate["baseline"]["snapshot"].is_object());
    assert_eq!(
        api.method(context, "session_export", json!({"session":session}))?["result"],
        original
    );
    assert_eq!(
        api.method(
            other,
            "session_accept_rebase",
            json!({"session":session,"revision":0,"review":candidate["review"]})
        )?["error"]["code"],
        "unknown_review"
    );
    assert_eq!(
        api.method(
            context,
            "session_accept_rebase",
            json!({"session":session,"revision":1,"review":candidate["review"]})
        )?["error"]["code"],
        "review_mismatch"
    );
    assert_eq!(api.method(context, "session_accept_rebase", json!({"session":session,"revision":0,"review":candidate["review"],"candidate":candidate}))?["error"]["code"], "invalid_request");
    let accepted = api.method(
        context,
        "session_accept_rebase",
        json!({"session":session,"revision":0,"review":candidate["review"]}),
    )?;
    assert_eq!(accepted["result"]["revision"], 1, "{accepted}");
    let rebased = api.method(context, "session_export", json!({"session":session}))?;
    assert_eq!(rebased["result"]["baseline"], candidate["baseline"]);
    assert_eq!(rebased["result"]["draft"], candidate["draft"]);
    assert_eq!(
        api.method(
            context,
            "review_close",
            json!({"review":candidate["review"]})
        )?["error"]["code"],
        "unknown_review"
    );
    let stale = api.method(
        context,
        "session_prepare_rebase",
        json!({"session":session,"revision":1}),
    )?["result"]["review"]
        .clone();
    assert!(stale.is_string());
    assert_eq!(
        api.method(
            context,
            "session_edit",
            json!({"session":session,"revision":1,"edit":disksgraph::PlanEdit::Clear})
        )?["result"]["revision"],
        2
    );
    assert_eq!(
        api.method(
            context,
            "session_accept_rebase",
            json!({"session":session,"revision":1,"review":stale})
        )?["error"]["detail"]["code"],
        "revision_conflict"
    );
    assert_eq!(
        api.method(context, "review_close", json!({"review":stale}))?["error"]["code"],
        "unknown_review"
    );
    let pending = api.method(
        context,
        "session_prepare_rebase",
        json!({"session":session,"revision":2}),
    )?["result"]["review"]
        .clone();
    api.method(context, "session_close", json!({"session":session}))?;
    assert_eq!(
        api.method(context, "review_close", json!({"review":pending}))?["error"]["code"],
        "unknown_review"
    );
    let marker = incident_marker()?;
    let path = scratch.0.join("active-attempt.json");
    fs::write(&path, marker.to_bytes()?)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let shown = api.method(context, "incident_review", json!({}))?;
    let shown = &shown["result"];
    assert!(shown["review"].is_string(), "{shown}");
    assert_eq!(shown["incident"]["attempt"], serde_json::to_value(&marker)?);
    let confirm = json!({"review":shown["review"],"run_id":marker.run_id,"token":shown["token"]});
    assert_eq!(
        api.method(other, "incident_acknowledge", confirm.clone())?["error"]["code"],
        "unknown_review"
    );
    let mut wrong = confirm.clone();
    wrong["token"] = json!(marker.plan_id);
    assert_eq!(
        api.method(context, "incident_acknowledge", wrong)?["error"]["code"],
        "review_mismatch"
    );
    let mut forged = confirm.clone();
    forged["policy"] = json!({"allow":true});
    assert_eq!(
        api.method(context, "incident_acknowledge", forged)?["error"]["code"],
        "invalid_request"
    );
    let refused = api.method(context, "incident_acknowledge", confirm)?;
    assert_eq!(
        refused["error"]["detail"]["code"], "incident_rejected",
        "{refused}"
    );
    assert_eq!(fs::read(&path)?, marker.to_bytes()?);
    assert_eq!(
        api.method(context, "review_close", json!({"review":shown["review"]}))?["error"]["code"],
        "unknown_review"
    );
    let pending = api.method(context, "incident_review", json!({}))?["result"]["review"].clone();
    assert!(pending.is_string());
    assert!(api
        .method(context, "review_close", json!({"review":pending}))?
        .get("error")
        .is_none());
    assert!(api.method(context, "incident_review", json!({}))?["result"]["review"].is_string());
    api.close(other)?;
    api.close(context)?;
    assert_eq!(fs::read(&path)?, marker.to_bytes()?);
    Ok(())
}

/// Synthetic historical marker for inspection/refusal only; these identities never select a native write.
fn incident_marker() -> Result<disksgraph::host::checkpoint::ActiveAttempt, Error> {
    use disksgraph::{
        host::{
            checkpoint::{ActiveAttempt, TargetFingerprint, CHECKPOINT_SCHEMA_VERSION},
            BackendId, LvmAction, ProviderAction,
        },
        model::{Bytes, ExternalId, LvmLvKind, NodeId},
    };
    let vg = NodeId::new();
    Ok(ActiveAttempt {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        run_id: "00000000-0000-7000-8000-000000000091".parse()?,
        plan_id: "00000000-0000-7000-8000-000000000092".parse()?,
        action_id: disksgraph::ActionId::new(0),
        operation_id: None,
        action: ProviderAction::Lvm(LvmAction::CreateLv {
            vg,
            planned_node_id: NodeId::new(),
            name: "consumer-data".into(),
            size: Bytes::new(16 * 1024 * 1024),
            layout: LvmLvKind::Linear,
        }),
        targets: vec![TargetFingerprint {
            planned_id: vg,
            external_ids: vec![ExternalId::LvmVgUuid("consumer-historical-vg".into())],
        }],
        plan_digest: "a".repeat(64),
        backend_id: BackendId::new("cli-unconfigured").ok_or("backend")?,
    })
}

/// The separately compiled write-capable test artifact must refuse a normal host before native setup.
#[test]
#[ignore = "requires cargo build -p disksgraph-ffi --example c7_vm_library"]
fn vm_library_refuses_an_unguarded_host() -> Result<(), Error> {
    let scratch = Scratch::new()?;
    let library = std::env::current_exe()?
        .parent()
        .ok_or("deps")?
        .parent()
        .ok_or("debug")?
        .join("examples/libc7_vm_library.so");
    let api = Api::load(&library, "disksgraph_vm")?;
    assert_eq!(api.version()?, 1);
    assert!(api.rejects_wrong_signature());
    let reply = api.open(&scratch.config())?;
    assert_eq!(reply["error"]["code"], "open_failed", "{reply}");
    assert_eq!(fs::read_dir(&scratch.0)?.count(), 0);
    Ok(())
}
