#![cfg(target_os = "linux")]
//! Cooperative lease tests touch only private temporary regular files and synthetic plans.

use std::{
    error::Error,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
};
use storage_core::model::Bytes;
use storage_engine::{
    ActionId, ExecutionRun,
    editing::{FrozenPlan, PlanSession},
    leases::{ExecutionLease, LeaseError},
};
use uuid::Uuid;

/// Owns a fresh private directory, removed only after all test guards have been dropped.
struct Scratch(PathBuf);
impl Scratch {
    /// Creates a unique app-owned lock root rather than using a shared writable temp root directly.
    fn new() -> Result<Self, Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!("pdisks-leases-{}", Uuid::now_v7()));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    /// Test-only cleanup after guard lifetimes end; production lock files must remain in place.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Creates a frozen fixture plan whose active first action is waiting for admission evidence.
fn fixture() -> Result<(FrozenPlan, ExecutionRun), Box<dyn Error>> {
    let baseline =
        serde_json::from_str(include_str!("../examples/fixtures/editing_baseline.json"))?;
    let mut session = PlanSession::new(baseline)?;
    let storage_provider::AllocationDomain::LvmVolumeGroup(vg) =
        session.baseline().capacity_evidence[0].domain;
    session.add_lv(session.revision(), vg, "data", Bytes::new(16 * 1024 * 1024))?;
    let frozen = session.freeze(session.revision())?;
    let mut run = ExecutionRun::new(frozen.document())?;
    run.begin_check(ActionId::new(0))?;
    Ok((frozen, run))
}

/// Independently opened handles and different plans contend on the same OS lock.
#[test]
fn host_lease_serializes_runs_and_releases_on_drop() -> Result<(), Box<dyn Error>> {
    let root = Scratch::new()?;
    let (_, run) = fixture()?;
    let (_, other) = fixture()?;
    let action = ActionId::new(0);
    let guard = ExecutionLease::try_acquire(&root.0, &run, action)?;
    assert_eq!(guard.run_id(), run.run_id());
    assert_eq!(guard.action(), action);
    assert_eq!(
        guard.held_resources(),
        run.context().plan.actions()[0]
            .informational_contract
            .affected_resources
    );
    assert!(matches!(
        ExecutionLease::try_acquire(&root.0, &other, action),
        Err(LeaseError::Busy)
    ));
    assert!(matches!(
        ExecutionLease::try_acquire(&root.0, &run, action),
        Err(LeaseError::Busy)
    ));
    let lock = root.0.join("execution.lock");
    assert_eq!(fs::metadata(&lock)?.len(), 0);
    assert_eq!(fs::metadata(&lock)?.permissions().mode() & 0o077, 0);
    drop(guard);
    assert!(lock.exists());
    let next = ExecutionLease::try_acquire(&root.0, &other, action)?;
    drop(next);
    Ok(())
}

/// Public roots, symlinks, and linked lock files cannot silently select another lock domain.
#[test]
fn lease_rejects_untrusted_paths_and_nonchecking_actions() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;
    let root = Scratch::new()?;
    let (frozen, run) = fixture()?;
    let action = ActionId::new(0);
    let waiting = ExecutionRun::new(frozen.document())?;
    assert!(matches!(
        ExecutionLease::try_acquire(&root.0, &waiting, action),
        Err(LeaseError::NotChecking)
    ));
    assert!(!root.0.join("execution.lock").exists());
    fs::set_permissions(&root.0, fs::Permissions::from_mode(0o777))?;
    assert!(matches!(
        ExecutionLease::try_acquire(&root.0, &run, action),
        Err(LeaseError::InsecureDirectory)
    ));
    fs::set_permissions(&root.0, fs::Permissions::from_mode(0o700))?;
    let target = root.0.join("untouched");
    fs::write(&target, b"keep")?;
    let lock = root.0.join("execution.lock");
    symlink(&target, &lock)?;
    assert!(ExecutionLease::try_acquire(&root.0, &run, action).is_err());
    assert_eq!(fs::read(&target)?, b"keep");
    fs::remove_file(&lock)?;
    fs::hard_link(&target, &lock)?;
    assert!(matches!(
        ExecutionLease::try_acquire(&root.0, &run, action),
        Err(LeaseError::InvalidLockFile)
    ));
    fs::remove_file(&lock)?;
    fs::create_dir(&lock)?;
    assert!(ExecutionLease::try_acquire(&root.0, &run, action).is_err());
    Ok(())
}

/// Builds test-only actual-state claims; production hosts must obtain these from a fresh probe.
fn observation(run: &ExecutionRun, instance: Uuid, epoch: u64) -> storage_engine::LiveObservation {
    storage_engine::LiveObservation {
        reference: storage_engine::ObservationReference {
            probe_instance: instance,
            epoch: storage_core::model::ProbeEpoch::new(epoch),
        },
        snapshot: run.document().baseline.snapshot.clone(),
    }
}

/// Synthetic prerequisite attestations test lifecycle wiring, never native checker correctness.
fn checks(
    run: &ExecutionRun,
    status: storage_engine::LiveCheckStatus,
) -> Vec<storage_engine::LiveCheck<storage_provider::ActionPrerequisite>> {
    run.context().plan.actions()[0]
        .informational_contract
        .prerequisites
        .iter()
        .cloned()
        .map(|condition| storage_engine::LiveCheck {
            condition,
            status: status.clone(),
        })
        .collect()
}

/// Resource attestation cannot cross runs, bypass unknown evidence, or release the live OS lock.
#[test]
fn leased_admission_binds_run_and_preserves_existing_evidence_rules() -> Result<(), Box<dyn Error>>
{
    use storage_engine::{ActionRunState, LiveCheckStatus};
    let root = Scratch::new()?;
    let (frozen, mut run) = fixture()?;
    let mut other = ExecutionRun::new(frozen.document())?;
    let action = ActionId::new(0);
    other.begin_check(action)?;
    let lease = ExecutionLease::try_acquire(&root.0, &run, action)?;
    let instance = Uuid::now_v7();
    let fresh = observation(&other, instance, 1);
    let claims = checks(&other, LiveCheckStatus::Satisfied);
    assert!(matches!(
        lease.record_admission(&mut other, fresh, LiveCheckStatus::Satisfied, claims),
        Err(LeaseError::RunMismatch)
    ));
    assert!(other.latest_observation().is_none());
    let fresh = observation(&run, instance, 1);
    let unknown = LiveCheckStatus::Unknown {
        required_evidence: "provider capability".into(),
    };
    let claims = checks(&run, LiveCheckStatus::Satisfied);
    assert!(!lease.record_admission(&mut run, fresh, unknown, claims)?);
    assert_eq!(run.state(action)?, ActionRunState::Checking);
    assert_eq!(
        run.admission(action)?
            .ok_or("admission missing")?
            .held_resources,
        lease.held_resources()
    );
    assert!(matches!(
        ExecutionLease::try_acquire(&root.0, &other, action),
        Err(LeaseError::Busy)
    ));
    let fresh = observation(&run, instance, 2);
    assert!(
        lease
            .record_admission(&mut run, fresh, LiveCheckStatus::Satisfied, Vec::new())
            .is_err()
    );
    assert_eq!(
        run.latest_observation()
            .ok_or("observation")?
            .reference
            .epoch
            .get(),
        1
    );
    let fresh = observation(&run, instance, 2);
    let claims = checks(&run, LiveCheckStatus::Satisfied);
    assert!(lease.record_admission(&mut run, fresh, LiveCheckStatus::Satisfied, claims)?);
    assert_eq!(run.state(action)?, ActionRunState::Running);
    assert!(matches!(
        ExecutionLease::try_acquire(&root.0, &other, action),
        Err(LeaseError::Busy)
    ));
    drop(lease);
    let next = ExecutionLease::try_acquire(&root.0, &other, action)?;
    drop(next);
    Ok(())
}

/// Intent is synced only under matching admission; exclusion survives completion and verification.
#[test]
fn lease_remains_held_through_journal_and_result_verification() -> Result<(), Box<dyn Error>> {
    use storage_engine::{
        BackendCompletion, BackendOutcome, LiveCheck, LiveCheckStatus, VerificationReport,
        journal::{DurableJournal, JournalLog, RecoveryStatus},
    };
    let root = Scratch::new()?;
    let (frozen, _) = fixture()?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let mut other = ExecutionRun::new(frozen.document())?;
    let path = root.0.join("run.jsonl");
    let mut journal = DurableJournal::create(&path, &frozen, &run)?;
    let other_path = root.0.join("other.jsonl");
    let mut other_journal = DurableJournal::create(&other_path, &frozen, &other)?;
    let action = ActionId::new(0);
    run.begin_check(action)?;
    other.begin_check(action)?;
    let lease = ExecutionLease::try_acquire(&root.0, &run, action)?;
    let before = fs::read(&path)?;
    assert!(lease.record_dispatch_intent(&mut journal, &run).is_err());
    assert_eq!(fs::read(&path)?, before);
    let instance = Uuid::now_v7();
    let observed = observation(&run, instance, 1);
    let claims = checks(&run, LiveCheckStatus::Satisfied);
    assert!(lease.record_admission(&mut run, observed, LiveCheckStatus::Satisfied, claims)?);
    assert!(matches!(
        lease.record_dispatch_intent(&mut journal, &other),
        Err(LeaseError::RunMismatch)
    ));
    let before_other = fs::read(&other_path)?;
    assert!(
        lease
            .record_dispatch_intent(&mut other_journal, &run)
            .is_err()
    );
    assert_eq!(fs::read(&other_path)?, before_other);
    lease.record_dispatch_intent(&mut journal, &run)?;
    assert_eq!(
        JournalLog::read(&path)?.review()?.actions[0].status,
        RecoveryStatus::InvocationUnconfirmed
    );
    let bytes = fs::read(&path)?;
    assert!(lease.record_dispatch_intent(&mut journal, &run).is_err());
    assert_eq!(fs::read(&path)?, bytes);
    let completion = BackendCompletion {
        run_id: run.run_id(),
        action,
        outcome: BackendOutcome::Succeeded,
        observation: observation(&run, instance, 2),
    };
    let reference = completion.observation.reference;
    run.record_completion(completion)?;
    journal.record_completion(&run, action)?;
    assert!(matches!(
        ExecutionLease::try_acquire(&root.0, &other, action),
        Err(LeaseError::Busy)
    ));
    // The synthetic verifier tests guard lifetime only, never native result evidence.
    let postconditions = run.context().plan.actions()[0]
        .informational_contract
        .postconditions
        .iter()
        .cloned()
        .map(|condition| LiveCheck {
            condition,
            status: LiveCheckStatus::Satisfied,
        })
        .collect();
    run.record_verification(VerificationReport {
        run_id: run.run_id(),
        action,
        observation: reference,
        checks: postconditions,
    })?;
    journal.flush_events(&run)?;
    assert!(matches!(
        ExecutionLease::try_acquire(&root.0, &other, action),
        Err(LeaseError::Busy)
    ));
    assert!(!run.is_verified());
    drop(lease);
    let next = ExecutionLease::try_acquire(&root.0, &other, action)?;
    drop(next);
    Ok(())
}
