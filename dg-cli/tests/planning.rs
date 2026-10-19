//! Command/direct API parity using the existing explicitly synthetic facade fixture.

#[path = "../../disksgraph/tests/support/mod.rs"]
mod support;

use dg_cli::{Command, execute, files};
use disksgraph::{
    DraftDocument, PlanDocument, PlanRequest,
    model::{Bytes, LvmLvKind, NodeKind},
};
use std::error::Error;

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
