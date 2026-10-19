#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]
//! Checked-import consumer for the separately built guarded VM library, with historical Rust parity.

#[path = "../../packaging/consumer/src/api.rs"]
mod dylib;
#[path = "../../dg-cli/examples/support/guest.rs"]
pub mod guest;
use disksgraph::{model::*, PlanEdit, PlanRequest};
use dylib::{Api, Error};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, Instant},
};

/// Calls the actual loaded library and requires its unambiguous success envelope.
fn method(api: &Api, context: u64, name: &str, params: Value) -> Result<Value, Error> {
    let response = api.method(context, name, params)?;
    if response.get("error").is_some() {
        return Err(format!("{name}: {response}").into());
    }
    Ok(response["result"].clone())
}

/// Edits, previews, freezes and starts the bounded native recipe through checked ABI imports only.
fn main() -> Result<(), Error> {
    guest::guard().map_err(|error| error.to_string())?;
    let case = std::env::args().nth(1).ok_or("guest case")?;
    if case != "success" && case != "mount-target-change" {
        return Err("unsupported guest case".into());
    }
    fs::write("/run/c7-case", &case)?;
    guest::setup("pvcreate", &["/dev/sda"]).map_err(|error| error.to_string())?;
    guest::setup("vgcreate", &["dg_c4", "/dev/sda"]).map_err(|error| error.to_string())?;
    let root = Path::new("/run/c4-execution");
    fs::create_dir(root)?;
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    let api = Api::load(Path::new("/usr/lib64/libc7_vm_library.so"), "disksgraph_vm")?;
    assert_eq!(api.version()?, 1);
    assert!(api.rejects_wrong_signature());
    let config =
        json!({"abi_version":1,"protocol_version":1,"root":root,"manifest":"/run/providers.json"});
    let opened = api.open(&config)?;
    let context = opened["result"]["context"]
        .as_u64()
        .ok_or_else(|| format!("open: {opened}"))?;
    let session = method(&api, context, "session_create", json!({}))?["session"].clone();
    let saved = method(&api, context, "session_export", json!({"session":session}))?;
    let document = disksgraph::DraftDocument::from_json(&saved.to_string())?;
    let vg = document
        .baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::LvmVg { .. }).then_some(*id))
        .ok_or("VG")?;
    let lv: NodeId = serde_json::from_value(method(&api, context, "session_edit", json!({"session":session,"revision":0,"edit":PlanEdit::Add(PlanRequest::CreateLv { vg, name:"data".into(), size:Bytes::new(64*1024*1024), layout:LvmLvKind::Linear, activate:true })}))?["created_node"].clone())?;
    let filesystem = serde_json::from_value(method(&api, context, "session_edit", json!({"session":session,"revision":1,"edit":PlanEdit::Add(PlanRequest::CreateFilesystem{backing:lv,filesystem:FilesystemKind::Ext4,label:None})}))?["created_node"].clone())?;
    method(
        &api,
        context,
        "session_edit",
        json!({"session":session,"revision":2,"edit":PlanEdit::Add(PlanRequest::Mount(MountEntry { source:MountSource::Filesystem(filesystem), target:"/mnt/c4".into(), options:disksgraph::host::EXT4_HOST_MOUNT_OPTIONS.iter().map(|value| (*value).into()).collect(), context:MountContext::Host }))}),
    )?;
    let saved = method(&api, context, "session_export", json!({"session":session}))?;
    let mut actual_preview = method(&api, context, "session_preview", json!({"session":session}))?;
    let frozen = method(
        &api,
        context,
        "session_freeze",
        json!({"session":session,"revision":3}),
    )?;
    let document = disksgraph::PlanDocument::from_json(&frozen.to_string())?;
    {
        // Saved-input parity does not load native libraries or supply an execution policy.
        let mut direct = disksgraph::native::open(root, None)?;
        let imported =
            direct.import_draft(disksgraph::DraftDocument::from_json(&saved.to_string())?)?;
        let mut expected = serde_json::to_value(direct.preview_document(imported)?)?;
        assert_eq!(
            serde_json::from_value::<Option<DesiredState>>(actual_preview["desired"].take())?,
            serde_json::from_value::<Option<DesiredState>>(expected["desired"].take())?
        );
        assert_eq!(actual_preview, expected);
        let mut expected = direct
            .freeze(imported, direct.revision(imported)?)?
            .document()
            .clone();
        expected.plan_id = document.plan_id;
        assert_eq!(expected.canonical_json()?, document.canonical_json()?);
    }
    // This private guest-owned file is explicit trusted host configuration, outside the ABI requests.
    fs::write(
        "/run/c7-review.json",
        serde_json::to_vec(&guest::ReviewedPlan {
            run: None,
            digest: disksgraph::host::checkpoint::plan_digest(&document)?,
            lv,
        })?,
    )?;
    fs::set_permissions("/run/c7-review.json", fs::Permissions::from_mode(0o600))?;
    let plan = method(&api, context, "plan_import", json!({"document":frozen}))?["plan"].clone();
    let job = method(&api, context, "apply_start", json!({"plan":plan}))?["job"].clone();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut since = 0;
    let mut delivered = Vec::new();
    let mut live = false;
    let terminal = loop {
        let progress = method(&api, context, "job_poll", json!({"job":job,"since":since}))?;
        assert_eq!(progress["events"]["gap"], false);
        delivered.extend(
            progress["events"]["events"]
                .as_array()
                .ok_or("events")?
                .iter()
                .cloned(),
        );
        since = progress["events"]["next_sequence"]
            .as_u64()
            .ok_or("cursor")?;
        if progress["finished"] == true {
            break progress;
        }
        live |= progress["snapshot"]["finished"] == false;
        assert!(Instant::now() < deadline, "job timeout: {progress}");
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(live, "no live snapshot was observed");
    assert_eq!(terminal["error"], Value::Null);
    let snapshot = &terminal["snapshot"];
    let run = &snapshot["run_id"];
    assert_eq!(
        method(&api, context, "run_snapshot", json!({"run":run}))?,
        *snapshot
    );
    assert_eq!(
        method(&api, context, "run_events", json!({"run":run,"since":0}))?["events"],
        json!(delivered)
    );
    assert_eq!(snapshot["finished"], true);
    let actions = snapshot["actions"].as_array().ok_or("actions")?;
    assert_eq!(actions.len(), 4);
    assert!(actions[..3]
        .iter()
        .all(|action| action["state"] == "verified"));
    assert_eq!(snapshot["source_map"], json!([]));
    for (index, action) in actions.iter().enumerate() {
        println!("C4_ACTION_{index}={}", action["state"]);
        println!("C4_BACKEND_{index}={}", action["outcome"]);
    }
    let marker = if case == "mount-target-change" {
        assert_eq!(snapshot["verified"], false, "{snapshot}");
        assert_eq!(actions[3]["state"], "outcome_unknown");
        let bytes = fs::read(root.join("active-attempt.json"))?;
        let marker = disksgraph::host::checkpoint::ActiveAttempt::from_bytes(&bytes)?;
        assert_eq!(marker.action_id, disksgraph::ActionId::new(3));
        assert_eq!(marker.backend_id.as_str(), "vm-mount");
        assert_eq!(marker.targets.len(), 3);
        let rejected = method(&api, context, "apply_blocking", json!({"plan":plan}))?;
        assert_eq!(rejected["error"]["code"], "checkpoint_blocked");
        assert_eq!(
            method(&api, context, "run_snapshot", json!({"run":run}))?,
            *snapshot
        );
        let inspection = method(&api, context, "incident_inspect", json!({}))?;
        assert_eq!(inspection["attempt"], serde_json::to_value(&marker)?);
        let review = method(&api, context, "incident_review", json!({}))?;
        let refused = api.method(
            context,
            "incident_acknowledge",
            json!({"review":review["review"],"run_id":marker.run_id,"token":review["token"]}),
        )?;
        assert_eq!(
            refused["error"]["detail"]["code"], "incident_rejected",
            "{refused}"
        );
        assert_eq!(fs::read(root.join("active-attempt.json"))?, bytes);
        println!("C7_INCIDENT_REFUSAL_VERIFIED");
        Some(bytes)
    } else {
        assert_eq!(snapshot["verified"], true, "{snapshot}");
        assert_eq!(actions[3]["state"], "verified");
        assert!(
            snapshot["goal_verification"]["observation"]["reference"]["epoch"].as_u64()
                > actions[3]["completion_observation"]["epoch"].as_u64()
        );
        assert!(!root.join("active-attempt.json").exists());
        fs::write("/mnt/c4/proof", b"C7 checked ABI real mounted ext4")?;
        assert_eq!(
            fs::read("/mnt/c4/proof")?,
            b"C7 checked ABI real mounted ext4"
        );
        None
    };
    method(&api, context, "job_close", json!({"job":job}))?;
    api.close(context)?;
    assert_eq!(fs::read_to_string("/run/c7-native-calls")?, "4");
    if let Some(bytes) = marker {
        assert_eq!(fs::read(root.join("active-attempt.json"))?, bytes);
        let reopened = api.open(&config)?;
        let next = reopened["result"]["context"]
            .as_u64()
            .ok_or_else(|| format!("reopen: {reopened}"))?;
        assert_ne!(next, context);
        let missing = api.method(next, "run_snapshot", json!({"run":run}))?;
        assert_eq!(missing["error"]["detail"]["code"], "unknown_run");
        let pending = method(&api, next, "incident_inspect", json!({}))?;
        assert_eq!(pending["attempt"], serde_json::from_slice::<Value>(&bytes)?);
        let imported =
            method(&api, next, "plan_import", json!({"document":frozen}))?["plan"].clone();
        let refused = method(&api, next, "apply_blocking", json!({"plan":imported}))?;
        assert_eq!(refused["error"]["code"], "checkpoint_blocked");
        api.close(next)?;
        assert_eq!(fs::read_to_string("/run/c7-native-calls")?, "0");
        assert_eq!(fs::read(root.join("active-attempt.json"))?, bytes);
        println!("C7_CLOSE_PRESERVATION_VERIFIED");
        println!("C4_EXPECTED_FAILURE_VERIFIED");
    } else {
        let files: Vec<_> = fs::read_dir(root)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<_, _>>()?;
        assert_eq!(files, [std::ffi::OsString::from("execution.lock")]);
        println!("C4_VERTICAL_VERIFIED");
    }
    println!("C7_LIVE_POLL_VERIFIED");
    println!("C7_ABI_VERIFIED");
    Ok(())
}
