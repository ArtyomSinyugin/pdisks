//! Incident protocol tests use only an existing recorded facade and private marker files.

#[path = "../../disksgraph/tests/support/mod.rs"]
mod support;

use disksgraph::{
    IncidentAcknowledgmentPolicy, IncidentInspection, LiveCheckStatus,
    host::{
        BackendId, LvmAction, ProviderAction,
        checkpoint::{ActiveAttempt, CHECKPOINT_SCHEMA_VERSION, TargetFingerprint},
    },
    model::{Bytes, ExternalId, LvmLvKind, NodeId},
};
use std::{
    error::Error,
    io::{BufRead, BufReader, Write},
    path::Path,
};

/// Deliberately synthetic safety decision: this fixture has no native operation or outside writer.
struct Policy {
    allow: bool,
    calls: usize,
}
impl IncidentAcknowledgmentPolicy for Policy {
    /// Counts independent policy evaluation separately from conscious token confirmation.
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

/// Publishes a valid private marker without executing or arming any disk operation.
fn marker(app: &mut support::Application, root: &Path) -> Result<ActiveAttempt, Box<dyn Error>> {
    let session = app.create_session()?;
    let baseline = app.export_draft(session)?.baseline;
    let vg = *baseline.snapshot.graph.nodes().next().ok_or("VG")?.0;
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
    dg_cli::files::save_document(
        &root.join("active-attempt.json"),
        std::str::from_utf8(&marker.to_bytes()?)?,
    )?;
    Ok(marker)
}

/// Exchanges the review and reply on real pipes, so the challenge cannot be known before review.
fn dialog(
    app: &mut support::Application,
    policy: &mut Policy,
    reply: impl FnOnce(&serde_json::Value) -> serde_json::Value + Send + 'static,
) -> Result<Result<serde_json::Value, dg_cli::Error>, Box<dyn Error>> {
    let (server_in, peer_out) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    let (peer_in, server_out) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    let peer = std::thread::spawn(move || -> std::io::Result<()> {
        let mut reader = BufReader::new(std::fs::File::from(peer_in));
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let review = serde_json::from_str(&line).map_err(std::io::Error::other)?;
        writeln!(std::fs::File::from(peer_out), "{}", reply(&review))?;
        Ok(())
    });
    let result = dg_cli::execute_with_io(
        app,
        &dg_cli::Command::AcknowledgeIncident,
        &mut BufReader::new(std::fs::File::from(server_in)),
        &mut std::fs::File::from(server_out),
        policy,
    );
    peer.join().map_err(|_| "incident peer panicked")??;
    Ok(result)
}

/// Confirmation mismatch, stale marker and independent policy refusal cannot remove the checkpoint.
#[test]
fn acknowledgment_requires_exact_review_and_independent_safety() -> Result<(), Box<dyn Error>> {
    for case in 0..6 {
        let (scratch, mut app, _, writes) = support::application()?;
        let original = marker(&mut app, &scratch.0)?;
        let bytes = std::fs::read(scratch.0.join("active-attempt.json"))?;
        let path = scratch.0.join("active-attempt.json");
        let mut policy = Policy {
            allow: case != 3,
            calls: 0,
        };
        let result = dialog(&mut app, &mut policy, move |review| {
            assert_eq!(review["type"], "incident_review");
            let mut reply = serde_json::json!({"command":"acknowledge", "run_id": review["incident"]["attempt"]["run_id"], "token":review["token"]});
            match case {
                0 => reply["run_id"] = serde_json::json!(uuid::Uuid::nil()),
                1 => reply["token"] = serde_json::json!(uuid::Uuid::nil()),
                2 => reply["extra"] = serde_json::json!(true),
                4 => {
                    let mut replacement = original;
                    replacement.plan_digest = "b".repeat(64);
                    std::fs::write(
                        path,
                        replacement
                            .to_bytes()
                            .unwrap_or_else(|error| panic!("replacement: {error}")),
                    )
                    .unwrap_or_else(|error| panic!("write: {error}"));
                }
                _ => {}
            }
            reply
        })?;
        if case == 5 {
            assert_eq!(result?["acknowledged"], true);
            assert!(!scratch.0.join("active-attempt.json").exists());
        } else {
            assert!(result.is_err());
            assert!(scratch.0.join("active-attempt.json").exists());
            if case != 4 {
                assert_eq!(std::fs::read(scratch.0.join("active-attempt.json"))?, bytes);
            }
        }
        assert_eq!(policy.calls, usize::from(case == 3 || case == 5));
        assert_eq!(writes.get(), 0);
    }
    Ok(())
}

/// Missing incidents and canceled/closed input cannot call the independent acknowledgment policy.
#[test]
fn empty_or_canceled_review_leaves_marker_untouched() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, probes, writes) = support::application()?;
    let mut policy = Policy {
        allow: true,
        calls: 0,
    };
    let mut output = Vec::new();
    let result =
        dg_cli::incident::acknowledge(&mut app, &mut b"".as_slice(), &mut output, &mut policy)?;
    assert!(result["incident"].is_null());
    assert!(output.is_empty());
    assert_eq!(probes.get(), 0);
    marker(&mut app, &scratch.0)?;
    for input in [b"".as_slice(), b"{\"command\":\"cancel\"}\n".as_slice()] {
        let mut input = input;
        let result = dg_cli::incident::acknowledge(&mut app, &mut input, &mut output, &mut policy)?;
        assert_eq!(result["acknowledged"], false);
        assert!(scratch.0.join("active-attempt.json").exists());
    }
    assert_eq!(policy.calls, 0);
    // A failed review writer stops before reading confirmation or clearing anything.
    let before = std::fs::read(scratch.0.join("active-attempt.json"))?;
    let mut failed_writer = std::fs::File::open(scratch.0.join("active-attempt.json"))?;
    assert!(matches!(
        dg_cli::execute_with_io(
            &mut app,
            &dg_cli::Command::AcknowledgeIncident,
            &mut b"".as_slice(),
            &mut failed_writer,
            &mut policy
        ),
        Err(dg_cli::Error::Io(_))
    ));
    assert_eq!(
        std::fs::read(scratch.0.join("active-attempt.json"))?,
        before
    );
    assert_eq!(policy.calls, 0);
    assert_eq!(writes.get(), 0);
    Ok(())
}
