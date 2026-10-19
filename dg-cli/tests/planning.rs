//! Command/direct API parity using the existing explicitly synthetic facade fixture.

#[path = "../../disksgraph/tests/support/mod.rs"]
mod support;

use dg_cli::{Command, files};
use disksgraph::{
    DraftDocument, PlanDocument, PlanRequest,
    model::{Bytes, LvmLvKind, NodeKind},
};
use std::error::Error;

/// Review, challenge and revision are checked within one process; only explicit save persists rebase.
#[test]
fn repl_rebase_requires_current_review_and_live_configuration() -> Result<(), Box<dyn Error>> {
    use std::io::{BufRead, BufReader, Write};
    let (scratch, mut app, probes, writes) = support::application()?;
    let path = scratch.0.join("rebase.json");
    execute(
        &mut app,
        &Command::New {
            output: path.clone(),
        },
    )?;
    let original = DraftDocument::from_json(&files::read_document(&path)?)?;
    let mut output = Vec::new();
    dg_cli::repl::edit(
        &mut app,
        &path,
        false,
        &mut b"{\"command\":\"prepare_rebase\"}\n".as_slice(),
        &mut output,
    )?;
    assert!(
        serde_json::from_slice::<serde_json::Value>(&output)?
            .get("error")
            .is_some()
    );
    assert_eq!(probes.get(), 1);
    let (server_in, client_out) = rustix::pipe::pipe()?;
    let (client_in, server_out) = rustix::pipe::pipe()?;
    let mut server = std::fs::File::from(server_out);
    let mut input = BufReader::new(std::fs::File::from(server_in));
    let peer = std::thread::spawn(move || -> std::io::Result<Vec<serde_json::Value>> {
        /// Exchanges one command/reply pair over a real duplex stream.
        fn exchange(
            writer: &mut impl Write,
            reader: &mut impl BufRead,
            command: serde_json::Value,
        ) -> std::io::Result<serde_json::Value> {
            writeln!(writer, "{command}")?;
            let mut line = String::new();
            reader.read_line(&mut line)?;
            serde_json::from_str(&line).map_err(std::io::Error::other)
        }
        let mut client = std::fs::File::from(client_out);
        let mut reader = BufReader::new(std::fs::File::from(client_in));
        let mut replies = Vec::new();
        let prepared = exchange(
            &mut client,
            &mut reader,
            serde_json::json!({"command":"prepare_rebase"}),
        )?;
        let token = prepared["result"]["token"].clone();
        replies.push(prepared);
        replies.push(exchange(
            &mut client,
            &mut reader,
            serde_json::json!({"command":"accept_rebase","token":uuid::Uuid::nil()}),
        )?);
        replies.push(exchange(
            &mut client,
            &mut reader,
            serde_json::json!({"command":"edit","edit":"clear"}),
        )?);
        replies.push(exchange(
            &mut client,
            &mut reader,
            serde_json::json!({"command":"accept_rebase","token":token}),
        )?);
        let next = exchange(
            &mut client,
            &mut reader,
            serde_json::json!({"command":"prepare_rebase"}),
        )?;
        let token = next["result"]["token"].clone();
        replies.push(next);
        replies.push(exchange(
            &mut client,
            &mut reader,
            serde_json::json!({"command":"accept_rebase","token":token}),
        )?);
        replies.push(exchange(
            &mut client,
            &mut reader,
            serde_json::json!({"command":"save"}),
        )?);
        writeln!(client, "{{\"command\":\"quit\"}}")?;
        Ok(replies)
    });
    let result = dg_cli::repl::edit(&mut app, &path, true, &mut input, &mut server)?;
    let replies = peer.join().map_err(|_| "peer panicked")??;
    assert_eq!(replies[1]["error"]["kind"], "usage");
    assert_eq!(replies[3]["error"]["detail"]["code"], "revision_conflict");
    assert!(replies[5].get("error").is_none());
    assert_eq!(result["saved"], true);
    let saved = DraftDocument::from_json(&files::read_document(&path)?)?;
    assert_ne!(saved.baseline.id, original.baseline.id);
    assert_eq!(saved.baseline.snapshot, original.baseline.snapshot);
    assert_eq!(probes.get(), 3);
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Undo/redo survive only within one REPL, and parsing errors never discard accepted requests.
#[test]
fn repl_keeps_history_and_requires_explicit_save() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, probes, writes) = support::application()?;
    let draft = scratch.0.join("repl.json");
    execute(
        &mut app,
        &Command::New {
            output: draft.clone(),
        },
    )?;
    let original = files::read_document(&draft)?;
    let saved = DraftDocument::from_json(&original)?;
    let vg = *saved.baseline.snapshot.graph.nodes().next().ok_or("VG")?.0;
    let request = serde_json::json!({"command":"edit", "edit": {"add": PlanRequest::CreateLv {
        vg, name: "repl-data".into(), size: Bytes::new(16 * 1024 * 1024),
        layout: LvmLvKind::Linear, activate: false,
    }}});
    let commands = format!(
        "{request}\n{{\"command\":\"preview\",\"unexpected\":true}}\n{{\"command\":\"undo\"}}\n{{\"command\":\"preview\"}}\n{{\"command\":\"redo\"}}\n{{\"command\":\"save\"}}\n{{\"command\":\"quit\"}}\n"
    );
    let mut output = Vec::new();
    let result = dg_cli::repl::edit(
        &mut app,
        &draft,
        false,
        &mut commands.as_bytes(),
        &mut output,
    )?;
    assert_eq!(result["saved"], true);
    let replies: Vec<serde_json::Value> = String::from_utf8(output)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(replies[1]["error"]["kind"], "document");
    assert_eq!(
        replies[3]["result"]["operations"]
            .as_array()
            .ok_or("operations")?
            .len(),
        0
    );
    let saved = DraftDocument::from_json(&files::read_document(&draft)?)?;
    assert_eq!(saved.draft.operations.len(), 1);
    let bytes = files::read_document(&draft)?;
    let result = dg_cli::repl::edit(
        &mut app,
        &draft,
        false,
        &mut b"{\"command\":\"edit\",\"edit\":\"clear\"}\n".as_slice(),
        &mut Vec::new(),
    )?;
    assert_eq!(result["saved"], false);
    assert_eq!(files::read_document(&draft)?, bytes);
    assert_eq!(probes.get(), 1);
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Discards the unused event stream for commands that only inspect or edit saved inputs.
fn execute(
    app: &mut support::Application,
    command: &Command,
) -> Result<serde_json::Value, dg_cli::Error> {
    dg_cli::execute(app, command, &mut std::io::sink())
}

/// Real executable invocations can edit and freeze a saved baseline without native registrations.
#[test]
fn binary_reopens_draft_in_separate_processes() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, _, writes) = support::application()?;
    let draft = scratch.0.join("draft.json");
    let request = scratch.0.join("request.json");
    let plan = scratch.0.join("plan.json");
    execute(
        &mut app,
        &Command::New {
            output: draft.clone(),
        },
    )?;
    let saved = DraftDocument::from_json(&files::read_document(&draft)?)?;
    let vg = saved.baseline.snapshot.graph.nodes().next().ok_or("VG")?.0;
    files::save_document(
        &request,
        &serde_json::to_string(&PlanRequest::CreateLv {
            vg: *vg,
            name: "process-data".into(),
            size: Bytes::new(16 * 1024 * 1024),
            layout: LvmLvKind::Linear,
            activate: false,
        })?,
    )?;
    for args in [
        vec![
            "plan",
            "add",
            "--draft",
            draft.to_str().ok_or("path")?,
            "--request",
            request.to_str().ok_or("path")?,
            "--output",
            draft.to_str().ok_or("path")?,
        ],
        vec!["plan", "preview", "--draft", draft.to_str().ok_or("path")?],
        vec![
            "plan",
            "freeze",
            "--draft",
            draft.to_str().ok_or("path")?,
            "--output",
            plan.to_str().ok_or("path")?,
        ],
    ] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_dg-cli"))
            .args(args)
            .arg("--root")
            .arg(&scratch.0)
            .output()?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        let _: serde_json::Value = serde_json::from_slice(&result.stdout)?;
        assert!(result.stderr.is_empty());
    }
    let reopened = DraftDocument::from_json(&files::read_document(&draft)?)?;
    assert_eq!(reopened.baseline, saved.baseline);
    assert_eq!(reopened.draft.operations.len(), 1);
    assert_eq!(
        PlanDocument::from_json(&files::read_document(&plan)?)?
            .actions
            .len(),
        1
    );
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Separate command invocations retain the exact saved baseline and deterministic action contracts.
#[test]
fn saved_workflow_matches_direct_api() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, probes, writes) = support::application()?;
    let draft = scratch.0.join("draft.json");
    let request = scratch.0.join("request.json");
    let plan = scratch.0.join("plan.json");
    execute(
        &mut app,
        &Command::New {
            output: draft.clone(),
        },
    )?;
    let original = DraftDocument::from_json(&files::read_document(&draft)?)?;
    let vg = original
        .baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::LvmVg { .. }).then_some(*id))
        .ok_or("VG")?;
    files::save_document(
        &request,
        &serde_json::to_string(&PlanRequest::CreateLv {
            vg,
            name: "data".into(),
            size: Bytes::new(16 * 1024 * 1024),
            layout: LvmLvKind::Linear,
            activate: true,
        })?,
    )?;
    execute(
        &mut app,
        &Command::Add {
            draft: draft.clone(),
            request,
            output: draft.clone(),
        },
    )?;
    let saved = DraftDocument::from_json(&files::read_document(&draft)?)?;
    assert_eq!(saved.baseline, original.baseline);
    assert_eq!(saved.draft.operations.len(), 1);
    let (_direct_root, mut direct, _, _) = support::application()?;
    let session = direct.import_draft(saved)?;
    assert_eq!(
        execute(
            &mut app,
            &Command::Preview {
                draft: draft.clone()
            }
        )?,
        serde_json::to_value(direct.preview_document(session)?)?
    );
    execute(
        &mut app,
        &Command::Freeze {
            draft,
            output: plan.clone(),
        },
    )?;
    let actual = PlanDocument::from_json(&files::read_document(&plan)?)?;
    let mut expected = direct
        .freeze(session, direct.revision(session)?)?
        .document()
        .clone();
    // Each freeze creates an independent plan identity; every execution input must otherwise match.
    assert_ne!(expected.plan_id, actual.plan_id);
    expected.plan_id = actual.plan_id;
    assert_eq!(actual.canonical_json()?, expected.canonical_json()?);
    assert_eq!(probes.get(), 1);
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Invalid forecasts survive process boundaries; failed freezes never replace previous output bytes.
#[test]
fn invalid_draft_is_saved_but_cannot_overwrite_plan() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, probes, writes) = support::application()?;
    let draft = scratch.0.join("draft.json");
    let request = scratch.0.join("request.json");
    let plan = scratch.0.join("plan.json");
    execute(
        &mut app,
        &Command::New {
            output: draft.clone(),
        },
    )?;
    let saved = DraftDocument::from_json(&files::read_document(&draft)?)?;
    let vg = saved.baseline.snapshot.graph.nodes().next().ok_or("VG")?.0;
    files::save_document(
        &request,
        &serde_json::to_string(&PlanRequest::CreateLv {
            vg: *vg,
            name: "too-large".into(),
            size: Bytes::new(128 * 1024 * 1024 * 1024),
            layout: LvmLvKind::Linear,
            activate: false,
        })?,
    )?;
    let result = execute(
        &mut app,
        &Command::Add {
            draft: draft.clone(),
            request,
            output: draft.clone(),
        },
    )?;
    assert_eq!(result["status"], "invalid");
    let preview = execute(
        &mut app,
        &Command::Preview {
            draft: draft.clone(),
        },
    )?;
    assert_eq!(preview["status"], "invalid");
    assert!(preview["desired"].is_null());
    files::save_document(&plan, "previous")?;
    assert!(matches!(
        execute(
            &mut app,
            &Command::Freeze {
                draft,
                output: plan.clone()
            }
        ),
        Err(dg_cli::Error::Application(disksgraph::DisksGraphError {
            code: disksgraph::ErrorCode::CannotFreeze,
            ..
        }))
    ));
    assert_eq!(files::read_document(&plan)?, "previous");
    assert_eq!(probes.get(), 1);
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// A command reports the same denied states as direct Rust and never invokes the fixture dispatcher.
#[test]
fn apply_preserves_failed_snapshot_and_separate_events() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, _, writes) = support::application()?;
    let session = app.create_session()?;
    let baseline = app.export_draft(session)?;
    let vg = *baseline
        .baseline
        .snapshot
        .graph
        .nodes()
        .next()
        .ok_or("VG")?
        .0;
    app.edit(
        session,
        app.revision(session)?,
        disksgraph::PlanEdit::Add(PlanRequest::CreateLv {
            vg,
            name: "denied".into(),
            size: Bytes::new(16 * 1024 * 1024),
            layout: LvmLvKind::Linear,
            activate: true,
        }),
    )?;
    let document = app
        .freeze(session, app.revision(session)?)?
        .document()
        .clone();
    let plan = scratch.0.join("plan.json");
    files::save_document(&plan, &document.canonical_json()?)?;
    let mut events = Vec::new();
    let result = dg_cli::execute(
        &mut app,
        &Command::Apply {
            file: plan.clone(),
            events: true,
        },
        &mut events,
    )?;
    let (direct_root, mut direct, _, direct_writes) = support::application()?;
    let error = direct
        .apply_blocking(document)
        .err()
        .ok_or("readiness must deny")?;
    let direct_snapshot = direct.run_snapshot(error.run_id.ok_or("retained attempt")?)?;
    assert_eq!(result["run"]["verified"], false);
    assert_eq!(result["run"]["finished"], true);
    assert_eq!(
        result["run"]["error"]["code"],
        serde_json::to_value(error.code)?
    );
    assert_eq!(
        result["run"]["actions"],
        serde_json::to_value(direct_snapshot.execution.actions)?
    );
    assert_eq!(
        result["run"]["plan_id"],
        direct_snapshot.execution.plan_id.to_string()
    );
    let lines: Vec<serde_json::Value> = String::from_utf8(events)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(lines[0]["type"], "replay");
    assert_eq!(lines[0]["gap"], false);
    assert!(result.get("events").is_none());
    assert!(!scratch.0.join("active-attempt.json").exists());
    assert!(!direct_root.0.join("active-attempt.json").exists());
    assert_eq!(writes.get() + direct_writes.get(), 0);

    // Corruption remains blocking; the CLI returns the retained error without clearing the marker.
    std::fs::write(scratch.0.join("active-attempt.json"), "corrupt")?;
    let result = execute(
        &mut app,
        &Command::Apply {
            file: plan,
            events: false,
        },
    )?;
    assert_eq!(result["run"]["error"]["code"], "checkpoint_blocked");
    assert_eq!(
        std::fs::read_to_string(scratch.0.join("active-attempt.json"))?,
        "corrupt"
    );
    assert!(execute(&mut app, &Command::Incident).is_err());
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Independent success is preserved even if the event consumer disconnects after the blocking run.
#[test]
fn completed_outcome_survives_event_writer_failure() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, _, writes) = support::application()?;
    let session = app.create_session()?;
    let document = app
        .freeze(session, app.revision(session)?)?
        .document()
        .clone();
    let plan = scratch.0.join("empty-plan.json");
    files::save_document(&plan, &document.canonical_json()?)?;
    let mut events = Vec::new();
    let result = dg_cli::execute(
        &mut app,
        &Command::Apply {
            file: plan.clone(),
            events: true,
        },
        &mut events,
    )?;
    assert_eq!(result["run"]["verified"], true);
    assert!(result["event_output_error"].is_null());
    assert!(String::from_utf8(events)?.lines().count() > 1);
    // A read-only file provides a real writer failure without a custom mock implementation.
    let mut failed_writer = std::fs::File::open(&plan)?;
    let result = dg_cli::execute(
        &mut app,
        &Command::Apply {
            file: plan,
            events: true,
        },
        &mut failed_writer,
    )?;
    assert_eq!(result["run"]["verified"], true);
    assert!(result["event_output_error"].is_string());
    assert_eq!(
        execute(&mut app, &Command::Incident)?,
        serde_json::json!({"incident": null})
    );
    assert_eq!(writes.get(), 0);
    Ok(())
}
