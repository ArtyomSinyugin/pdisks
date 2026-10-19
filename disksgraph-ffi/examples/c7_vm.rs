#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]
//! Checked-import consumer for the separately built guarded VM library, with historical Rust parity.

#[path = "../tests/support/dylib.rs"]
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
    if case != "success" {
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
    let opened = api.open(
        &json!({"abi_version":1,"protocol_version":1,"root":root,"manifest":"/run/providers.json"}),
    )?;
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
    assert_eq!(snapshot["verified"], true, "{snapshot}");
    let actions = snapshot["actions"].as_array().ok_or("actions")?;
    assert_eq!(actions.len(), 4);
    assert!(actions.iter().all(|action| action["state"] == "verified"));
    assert_eq!(snapshot["source_map"], json!([]));
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
    method(&api, context, "job_close", json!({"job":job}))?;
    api.close(context)?;
    assert_eq!(fs::read_to_string("/run/c7-native-calls")?, "4");
    println!("C7_LIVE_POLL_VERIFIED");
    println!("C7_ABI_VERIFIED");
    println!("C4_VERTICAL_VERIFIED");
    Ok(())
}
