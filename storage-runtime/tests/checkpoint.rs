//! Checkpoint serialization uses synthetic plans and never authorizes a native write.

use std::error::Error;
use storage_core::model::{Bytes, ExternalId};
use storage_engine::{
    ActionId, AdmissionReport, ExecutionRun, LiveCheck, LiveCheckStatus, LiveObservation,
    ObservationReference, editing::PlanSession,
};
use storage_provider::{BackendId, FilesystemAction, LvmAction, ProviderAction};
use storage_runtime::checkpoint::{
    ActiveAttempt, CheckpointError, MAX_CHECKPOINT_BYTES, TargetFingerprint,
};
use uuid::Uuid;

/// Builds a synthetic admitted LV creation, with no OS resources or authorization claims.
fn admitted_run() -> Result<ExecutionRun, Box<dyn Error>> {
    let source: storage_probe::ProbeSnapshot = serde_json::from_str(include_str!(
        "../../storage-engine/examples/fixtures/probe_snapshot.json"
    ))?;
    let baseline =
        storage_engine::HistoricalBaseline::from_lvm_observations(source.current, &source.lvm_vgs)?;
    let mut session = PlanSession::new(baseline)?;
    let storage_provider::AllocationDomain::LvmVolumeGroup(vg) =
        session.baseline().capacity_evidence[0].domain;
    session.add_lv(session.revision(), vg, "data", Bytes::new(16 * 1024 * 1024))?;
    let frozen = session.freeze(session.revision())?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let action = ActionId::new(0);
    run.begin_check(action)?;
    let contract = &run.context().plan.actions()[0].informational_contract;
    let report = AdmissionReport {
        run_id: run.run_id(),
        action,
        observation: LiveObservation {
            reference: ObservationReference {
                probe_instance: Uuid::now_v7(),
                epoch: storage_core::model::ProbeEpoch::new(1),
            },
            snapshot: run.document().baseline.snapshot.clone(),
        },
        applicability: LiveCheckStatus::Satisfied,
        held_resources: contract.affected_resources.clone(),
        checks: contract
            .prerequisites
            .iter()
            .cloned()
            .map(|condition| LiveCheck {
                condition,
                status: LiveCheckStatus::Satisfied,
            })
            .collect(),
    };
    assert!(run.record_admission(report)?);
    Ok(run)
}

/// Captures one fixture VG fingerprint; actual resolution belongs to the future trusted host.
fn attempt(run: &ExecutionRun) -> Result<ActiveAttempt, Box<dyn Error>> {
    let ProviderAction::Lvm(LvmAction::CreateLv { vg, .. }) =
        &run.context().plan.actions()[0].operation
    else {
        return Err("expected LV creation".into());
    };
    let node = run
        .document()
        .baseline
        .snapshot
        .graph
        .node(vg)
        .ok_or("VG")?;
    Ok(ActiveAttempt::capture(
        run,
        ActionId::new(0),
        None,
        vec![TargetFingerprint {
            planned_id: *vg,
            external_ids: node.size.identities.clone(),
        }],
        BackendId::new("libblockdev-lvm").ok_or("backend ID")?,
    )?)
}

/// Captured data names the exact plan/action/backend without recording a graph or runtime phase.
#[test]
fn captures_exact_admitted_action_and_round_trips() -> Result<(), Box<dyn Error>> {
    let mut run = admitted_run()?;
    let checkpoint = attempt(&run)?;
    assert_eq!(checkpoint.run_id, run.run_id());
    assert_eq!(checkpoint.plan_id, run.document().plan_id);
    assert_eq!(checkpoint.action, run.context().plan.actions()[0].operation);
    assert_eq!(checkpoint.backend_id.as_str(), "libblockdev-lvm");
    assert_eq!(checkpoint.plan_digest, attempt(&run)?.plan_digest);
    let bytes = checkpoint.to_bytes()?;
    assert_eq!(ActiveAttempt::from_bytes(&bytes)?, checkpoint);
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    for field in [
        "phase",
        "events",
        "baseline",
        "snapshot",
        "password",
        "passphrase",
    ] {
        assert!(value.get(field).is_none());
    }
    run.mark_dispatch_started(ActionId::new(0))?;
    assert!(matches!(
        attempt(&run)
            .err()
            .and_then(|e| e.downcast::<CheckpointError>().ok())
            .as_deref(),
        Some(CheckpointError::NotAdmitted)
    ));
    Ok(())
}

/// A changed immutable plan identity changes its digest even if its operations are identical.
#[test]
fn digest_binds_the_canonical_plan() -> Result<(), Box<dyn Error>> {
    let run = admitted_run()?;
    let checkpoint = attempt(&run)?;
    let mut document = run.document().clone();
    document.plan_id = Uuid::now_v7();
    let mut other = ExecutionRun::new(&document)?;
    other.begin_check(ActionId::new(0))?;
    let mut admission = run.admission(ActionId::new(0))?.ok_or("admission")?.clone();
    admission.run_id = other.run_id();
    other.record_admission(admission)?;
    assert_ne!(attempt(&other)?.plan_digest, checkpoint.plan_digest);
    assert_eq!(attempt(&other)?.action, checkpoint.action);
    Ok(())
}

/// Damaged envelopes, unrecognized phases, and nested action extensions never become absence.
#[test]
fn rejects_corruption_versions_and_unknown_fields() -> Result<(), Box<dyn Error>> {
    let checkpoint = attempt(&admitted_run()?)?;
    let value = serde_json::to_value(&checkpoint)?;
    for case in 0..7 {
        let mut changed = value.clone();
        match case {
            0 => changed["schema_version"] = serde_json::json!(2),
            1 => changed["run_id"] = serde_json::json!(Uuid::nil()),
            2 => changed["plan_id"] = serde_json::json!(Uuid::nil()),
            3 => changed["plan_digest"] = serde_json::json!("invalid"),
            4 => changed["phase"] = serde_json::json!("verified"),
            5 => changed["targets"][0]["path"] = serde_json::json!("/dev/dm-7"),
            _ => {
                changed["action"]["operation"]["create_lv"]["passphrase"] =
                    serde_json::json!("must be rejected")
            }
        }
        assert!(
            ActiveAttempt::from_bytes(&serde_json::to_vec(&changed)?).is_err(),
            "case {case}"
        );
    }
    let bytes = checkpoint.to_bytes()?;
    assert!(ActiveAttempt::from_bytes(&bytes[..bytes.len() / 2]).is_err());
    assert!(ActiveAttempt::from_bytes(b"{}").is_err());
    assert!(matches!(
        ActiveAttempt::from_bytes(&vec![b' '; MAX_CHECKPOINT_BYTES + 1]),
        Err(CheckpointError::TooLarge)
    ));
    Ok(())
}

/// Path-only, empty, duplicate, and oversized fingerprints cannot be persisted as stable bindings.
#[test]
fn rejects_missing_identity_and_bounds_nested_data() -> Result<(), Box<dyn Error>> {
    let checkpoint = attempt(&admitted_run()?)?;
    for case in 0..8 {
        let mut changed = checkpoint.clone();
        match case {
            0 => changed.targets.clear(),
            1 => changed.targets[0].external_ids.clear(),
            2 => changed.targets.push(changed.targets[0].clone()),
            3 => {
                changed.targets[0].external_ids = vec![ExternalId::Serial {
                    vendor: None,
                    value: "not a VG UUID".into(),
                }]
            }
            4 => changed.targets[0].external_ids = vec![ExternalId::LvmVgUuid(" ".into())],
            5 => changed.targets[0].external_ids = vec![ExternalId::LvmVgUuid("x".repeat(1025))],
            6 => {
                changed.targets[0].external_ids =
                    vec![changed.targets[0].external_ids[0].clone(); 17]
            }
            _ => changed.targets = vec![changed.targets[0].clone(); 65],
        }
        assert!(changed.to_bytes().is_err(), "case {case}");
    }
    Ok(())
}

/// An action without reviewed persistence rules is rejected rather than dumping arbitrary options.
#[test]
fn rejects_unreviewed_actions_and_accepts_lv_activation() -> Result<(), Box<dyn Error>> {
    let mut checkpoint = attempt(&admitted_run()?)?;
    let target = checkpoint.targets[0].planned_id;
    checkpoint.action = ProviderAction::Filesystem(FilesystemAction::SetLabel {
        filesystem: target,
        label: Some("private".into()),
    });
    assert!(matches!(
        checkpoint.to_bytes(),
        Err(CheckpointError::UnsupportedAction)
    ));
    checkpoint.action = ProviderAction::Lvm(LvmAction::SetActivation {
        target,
        active: true,
    });
    assert!(checkpoint.to_bytes().is_err());
    checkpoint.targets[0].external_ids = vec![ExternalId::LvmLvUuid("observed-lv-uuid".into())];
    assert_eq!(
        ActiveAttempt::from_bytes(&checkpoint.to_bytes()?)?,
        checkpoint
    );
    Ok(())
}

/// Ext4 intent retains only reviewed parameters and the pre-dispatch LV UUID, never a guessed FS UUID.
#[test]
fn ext4_checkpoint_requires_lv_identity_and_bounded_label() -> Result<(), Box<dyn Error>> {
    let mut checkpoint = attempt(&admitted_run()?)?;
    let backing = checkpoint.targets[0].planned_id;
    let planned_node_id = storage_core::model::NodeId::new();
    checkpoint.action = ProviderAction::Filesystem(FilesystemAction::Create {
        backing,
        planned_node_id,
        filesystem: storage_core::model::FilesystemKind::Ext4,
        label: Some("data".into()),
    });
    assert!(checkpoint.to_bytes().is_err());
    checkpoint.targets[0].external_ids = vec![ExternalId::LvmLvUuid("verified-lv".into())];
    assert_eq!(
        ActiveAttempt::from_bytes(&checkpoint.to_bytes()?)?,
        checkpoint
    );
    for label in ["x".repeat(17), "bad\0label".into()] {
        let mut changed = checkpoint.clone();
        if let ProviderAction::Filesystem(FilesystemAction::Create { label: actual, .. }) =
            &mut changed.action
        {
            *actual = Some(label);
        }
        assert!(changed.to_bytes().is_err());
    }
    let mut changed = serde_json::to_value(&checkpoint)?;
    changed["action"]["operation"]["create"]["password"] = serde_json::json!("reject");
    assert!(ActiveAttempt::from_bytes(&serde_json::to_vec(&changed)?).is_err());
    Ok(())
}
