//! Guarded native VM workflow through the direct Rust facade or CLI adapter.
#[path = "support/guest.rs"]
mod guest;
use disksgraph::host::{
    authorization::AuthorizationContext,
    capability::{BackendCapabilityCheck, OperationAvailability},
    checkpoint::{ActiveAttempt, plan_digest},
    dispatcher::BackendDispatcher,
    host::LinuxLiveHost,
};
use disksgraph::model::*;
use disksgraph::{ActionId, ActionRunState, DisksGraph, PlanEdit, PlanRequest};
use guest::*;
use std::{error::Error, fs, path::Path};

/// Confirms the real incident consciously while retaining the executable's conservative checks.
fn refuse_cli_acknowledgment<
    C: BackendCapabilityCheck,
    A: AuthorizationContext,
    D: BackendDispatcher,
>(
    app: &mut DisksGraph<LinuxLiveHost<'_, C, A>, D>,
) -> Result<(), Box<dyn Error>> {
    use std::io::{BufRead, BufReader, Write};
    let (server_in, peer_out) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    let (peer_in, server_out) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    let peer = std::thread::spawn(move || -> std::io::Result<()> {
        let mut reader = BufReader::new(fs::File::from(peer_in));
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let review: serde_json::Value =
            serde_json::from_str(&line).map_err(std::io::Error::other)?;
        assert_eq!(review["type"], "incident_review");
        writeln!(
            fs::File::from(peer_out),
            "{}",
            serde_json::json!({
                "command":"acknowledge", "run_id":review["incident"]["attempt"]["run_id"], "token":review["token"],
            })
        )?;
        Ok(())
    });
    let before = fs::read("/run/c4-execution/active-attempt.json")?;
    let mut policy = dg_cli::incident::UnconfiguredPolicy;
    let result = dg_cli::execute_with_io(
        app,
        &dg_cli::Command::AcknowledgeIncident,
        &mut BufReader::new(fs::File::from(server_in)),
        &mut fs::File::from(server_out),
        &mut policy,
    );
    peer.join()
        .map_err(|_| "incident confirmation peer panicked")??;
    // GuestProbe deliberately regenerates NodeIds each pass; Changed may refuse before the
    // default Unknown safety policy. Neither condition permits removing the native checkpoint.
    assert!(matches!(
        result,
        Err(dg_cli::Error::Application(disksgraph::DisksGraphError {
            code: disksgraph::ErrorCode::IncidentRejected,
            ..
        }))
    ));
    assert_eq!(fs::read("/run/c4-execution/active-attempt.json")?, before);
    println!("C6_INCIDENT_REFUSAL_VERIFIED");
    Ok(())
}

/// Runs exactly CreateLv, activation, ext4 and Host Mount through the normal coordinator.
fn main() -> Result<(), Box<dyn Error>> {
    guard()?;
    let mut args = std::env::args().skip(1);
    let fail_mount = match args.next().as_deref() {
        None | Some("success") => false,
        Some("mount-target-change") => true,
        _ => return Err("unknown guest experiment".into()),
    };
    let cli = match args.next().as_deref() {
        None | Some("rust") => false,
        Some("cli") => true,
        _ => return Err("unknown guest interface".into()),
    };
    if args.next().is_some() {
        return Err("unexpected guest arguments".into());
    }
    println!("C4_STAGE=setup");
    setup("pvcreate", &["/dev/sda"])?;
    setup("vgcreate", &["dg_c4", "/dev/sda"])?;
    fs::create_dir("/run/c4-execution")?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions("/run/c4-execution", fs::Permissions::from_mode(0o700))?;
    }
    let reviewed = std::rc::Rc::new(std::cell::RefCell::new(None));
    guest::with_host(fail_mount, reviewed.clone(), None, |mut app, calls| {
        let observed = app.probe()?;
        assert!(
            !observed
                .snapshot
                .graph
                .nodes()
                .any(|(_, node)| matches!(node.kind.kind, NodeKind::LvmLv(_)))
        );
        let session = app.create_session()?;
        let baseline = app.export_draft(session)?.baseline;
        let vg = baseline
            .snapshot
            .graph
            .nodes()
            .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::LvmVg { .. }).then_some(*id))
            .ok_or("private VG")?;
        let lv = app
            .edit(
                session,
                app.revision(session)?,
                PlanEdit::Add(PlanRequest::CreateLv {
                    vg,
                    name: "data".into(),
                    size: Bytes::new(64 * 1024 * 1024),
                    layout: LvmLvKind::Linear,
                    activate: true,
                }),
            )?
            .created_node
            .ok_or("LV output")?;
        let filesystem = app
            .edit(
                session,
                app.revision(session)?,
                PlanEdit::Add(PlanRequest::CreateFilesystem {
                    backing: lv,
                    filesystem: FilesystemKind::Ext4,
                    label: None,
                }),
            )?
            .created_node
            .ok_or("filesystem output")?;
        app.edit(
            session,
            app.revision(session)?,
            PlanEdit::Add(PlanRequest::Mount(MountEntry {
                source: MountSource::Filesystem(filesystem),
                target: "/mnt/c4".into(),
                options: disksgraph::host::EXT4_HOST_MOUNT_OPTIONS
                    .iter()
                    .map(|option| (*option).into())
                    .collect(),
                context: MountContext::Host,
            })),
        )?;
        assert!(matches!(
            app.preview(session)?.status(),
            disksgraph::PreviewStatus::Conditional | disksgraph::PreviewStatus::Complete
        ));
        let frozen = app.freeze(session, app.revision(session)?)?;
        let plan_path = Path::new("/run/c4-plan.json");
        let document = if cli {
            let draft_path = Path::new("/run/c6-draft.json");
            dg_cli::files::save_document(draft_path, &app.export_draft(session)?.to_json()?)?;
            let mut preview = dg_cli::execute(
                &mut app,
                &dg_cli::Command::Preview {
                    draft: draft_path.into(),
                },
                &mut Vec::new(),
            )?;
            let mut expected = serde_json::to_value(app.preview_document(session)?)?;
            // Graph sets may serialize in a different order after import; compare their typed content.
            assert_eq!(
                serde_json::from_value::<Option<DesiredState>>(preview["desired"].take())?,
                serde_json::from_value::<Option<DesiredState>>(expected["desired"].take())?,
            );
            assert_eq!(preview, expected);
            dg_cli::execute(
                &mut app,
                &dg_cli::Command::Freeze {
                    draft: draft_path.into(),
                    output: plan_path.into(),
                },
                &mut Vec::new(),
            )?;
            let document =
                disksgraph::PlanDocument::from_json(&dg_cli::files::read_document(plan_path)?)?;
            // Freeze allocates a new document identity; all executable inputs must otherwise be identical.
            let mut expected = frozen.document().clone();
            expected.plan_id = document.plan_id;
            assert_eq!(expected.canonical_json()?, document.canonical_json()?);
            document
        } else {
            let document = frozen.document().clone();
            dg_cli::files::save_document(plan_path, &document.canonical_json()?)?;
            document
        };
        // This explicit VM-only review is supplied through host-owned state, never imported approval JSON.
        *reviewed.borrow_mut() = Some(ReviewedPlan {
            run: None,
            digest: plan_digest(&document)?,
            lv,
        });
        let operations: Vec<_> = document
            .actions
            .iter()
            .map(|action| action.operation.clone())
            .collect();
        let capability = app.capabilities(&operations)?;
        assert!(
            capability
                .operations
                .iter()
                .all(|entry| matches!(entry.availability, OperationAvailability::Available(_)))
        );
        println!("C4_STAGE=facade_apply");
        let run = if cli {
            let mut stream = Vec::new();
            let result = dg_cli::execute(
                &mut app,
                &dg_cli::Invocation::parse(
                    [
                        "plan",
                        "apply",
                        "--file",
                        "/run/c4-plan.json",
                        "--root",
                        "/run/c4-execution",
                        "--manifest",
                        "/run/providers.json",
                        "--events",
                        "jsonl",
                    ]
                    .map(std::ffi::OsString::from),
                )?
                .command,
                &mut stream,
            )?;
            assert_eq!(
                dg_cli::result_exit_code(&result),
                if fail_mount { 3 } else { 0 }
            );
            assert!(result["event_output_error"].is_null());
            let run = serde_json::from_value(result["run"]["run_id"].clone())?;
            assert_eq!(result["run"], serde_json::to_value(app.run_snapshot(run)?)?);
            let mut lines = std::str::from_utf8(&stream)?.lines();
            let header: serde_json::Value =
                serde_json::from_str(lines.next().ok_or("live header")?)?;
            assert_eq!(header["type"], "stream");
            let delivered: Vec<disksgraph::RunEvent> =
                lines.map(serde_json::from_str).collect::<Result<_, _>>()?;
            assert_eq!(delivered, app.events(run, 0)?.events);
            println!("C6_CLI_EVENTS={}", delivered.len());
            run
        } else {
            app.apply_frozen(&frozen)?
        };
        let status = app.run_snapshot(run)?;
        assert!(status.execution.finished);
        assert!(status.error.is_none());
        if cli {
            assert!(status.source_map.is_empty());
        } else {
            assert_eq!(status.source_map, frozen.source_map());
        }
        assert_eq!(
            frozen
                .source_map()
                .iter()
                .map(|entry| entry.actions.len())
                .sum::<usize>(),
            4
        );
        let events = app.events(run, 0)?;
        assert!(!events.gap);
        assert_eq!(events.next_sequence, status.execution.event_sequence);
        assert!(app.events(run, events.next_sequence)?.events.is_empty());
        println!("C5_EVENTS={}", events.events.len());
        for action in &status.execution.actions {
            println!("C4_ACTION_{}={:?}", action.action.get(), action.state);
            println!("C4_BACKEND_{}={:?}", action.action.get(), action.outcome);
        }
        println!(
            "C4_CHECKPOINT={}",
            Path::new("/run/c4-execution/active-attempt.json").exists()
        );
        println!("C4_RESULT_VERIFIED={}", status.execution.verified);
        assert_eq!(calls.get(), 4);
        for index in 0..3 {
            assert_eq!(
                status.execution.actions[index].state,
                ActionRunState::Verified
            );
        }
        if fail_mount {
            assert!(!status.execution.verified);
            assert_eq!(
                status.execution.actions[3].state,
                ActionRunState::OutcomeUnknown
            );
            let marker =
                ActiveAttempt::from_bytes(&fs::read("/run/c4-execution/active-attempt.json")?)?;
            assert_eq!(marker.action_id, ActionId::new(3));
            assert_eq!(marker.backend_id.as_str(), "vm-mount");
            assert_eq!(marker.targets.len(), 3);
            // A new attempt on either public route is blocked before another native invocation.
            if cli {
                let rejected = dg_cli::execute(
                    &mut app,
                    &dg_cli::Command::Apply {
                        file: plan_path.into(),
                        events: false,
                    },
                    &mut Vec::new(),
                )?;
                assert_eq!(dg_cli::result_exit_code(&rejected), 3);
                assert_eq!(rejected["run"]["error"]["code"], "checkpoint_blocked");
                assert_eq!(
                    app.run_snapshot(run)?.execution.actions,
                    status.execution.actions
                );
                let inspection =
                    dg_cli::execute(&mut app, &dg_cli::Command::Incident, &mut Vec::new())?;
                assert_eq!(
                    inspection["incident"]["attempt"],
                    serde_json::to_value(&marker)?
                );
                refuse_cli_acknowledgment(&mut app)?;
            } else {
                let rejected = app
                    .apply_blocking(document.clone())
                    .err()
                    .ok_or("incident must block another attempt")?;
                assert_eq!(rejected.code, disksgraph::ErrorCode::CheckpointBlocked);
            }
            assert_eq!(calls.get(), 4);
            assert!(app.incident()?.is_some());
            if cli {
                println!("C6_CLI_VERIFIED");
            }
            println!("C5_FACADE_VERIFIED");
            println!("C4_EXPECTED_FAILURE_VERIFIED");
            return Ok(());
        }
        if !status.execution.verified {
            return Err("facade vertical slice did not independently verify".into());
        }
        assert_eq!(status.execution.actions[3].state, ActionRunState::Verified);
        assert!(!Path::new("/run/c4-execution/active-attempt.json").exists());
        assert!(
            status
                .execution
                .goal_verification
                .as_ref()
                .ok_or("final report")?
                .observation
                .reference
                .epoch
                > status.execution.actions[3]
                    .completion_observation
                    .ok_or("mount completion")?
                    .epoch
        );
        fs::write("/mnt/c4/proof", b"C5 facade real mounted ext4")?;
        assert_eq!(fs::read("/mnt/c4/proof")?, b"C5 facade real mounted ext4");
        if cli {
            println!("C6_CLI_VERIFIED");
        }
        println!("C5_FACADE_VERIFIED");
        println!("C4_VERTICAL_VERIFIED");
        Ok(())
    })
}
