#![cfg(target_os = "linux")]
//! Cooperative lease tests touch only private temporary regular files and synthetic plans.

use std::{
    error::Error,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    sync::Mutex,
};
use storage_core::model::Bytes;
use storage_engine::{
    ActionId, ExecutionRun,
    editing::{FrozenPlan, PlanSession},
};
use storage_runtime::lease::{ExecutionLease, ExecutionRoot, LeaseError};
use uuid::Uuid;

/// Fork temporarily inherits other threads' CLOEXEC fds before exec; isolate release assertions.
static PROCESS_TESTS: Mutex<()> = Mutex::new(());

/// Owns a fresh private directory, removed only after all test guards have been dropped.
struct Scratch(PathBuf);
impl Scratch {
    /// Creates a unique app-owned lock root rather than using a shared writable temp root directly.
    fn new() -> Result<Self, Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!("disksgraph-leases-{}", Uuid::now_v7()));
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
    let baseline = serde_json::from_str(include_str!(
        "../../storage-engine/examples/fixtures/editing_baseline.json"
    ))?;
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
    let _process = PROCESS_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = Scratch::new()?;
    let (_, run) = fixture()?;
    let (_, other) = fixture()?;
    let action = ActionId::new(0);
    let guard = ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &run, action)?;
    assert_eq!(guard.run_id(), run.run_id());
    assert_eq!(guard.action(), action);
    assert_eq!(
        guard.held_resources(),
        run.context().plan.actions()[0]
            .informational_contract
            .affected_resources
    );
    assert!(matches!(
        ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &other, action),
        Err(LeaseError::Busy)
    ));
    assert!(matches!(
        ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &run, action),
        Err(LeaseError::Busy)
    ));
    let lock = root.0.join("execution.lock");
    assert_eq!(fs::metadata(&lock)?.len(), 0);
    assert_eq!(fs::metadata(&lock)?.permissions().mode() & 0o077, 0);
    drop(guard);
    assert!(lock.exists());
    let next = ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &other, action)?;
    drop(next);
    Ok(())
}

/// Public roots, symlinks, and linked lock files cannot silently select another lock domain.
#[test]
fn lease_rejects_untrusted_paths_and_nonchecking_actions() -> Result<(), Box<dyn Error>> {
    let _process = PROCESS_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    use std::os::unix::fs::symlink;
    let root = Scratch::new()?;
    let (frozen, run) = fixture()?;
    let action = ActionId::new(0);
    let waiting = ExecutionRun::new(frozen.document())?;
    assert!(matches!(
        ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &waiting, action),
        Err(LeaseError::NotChecking)
    ));
    assert!(!root.0.join("execution.lock").exists());
    fs::set_permissions(&root.0, fs::Permissions::from_mode(0o777))?;
    assert!(matches!(
        ExecutionRoot::open(&root.0),
        Err(LeaseError::InsecureDirectory)
    ));
    fs::set_permissions(&root.0, fs::Permissions::from_mode(0o700))?;
    let target = root.0.join("untouched");
    fs::write(&target, b"keep")?;
    let lock = root.0.join("execution.lock");
    symlink(&target, &lock)?;
    assert!(ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &run, action).is_err());
    assert_eq!(fs::read(&target)?, b"keep");
    fs::remove_file(&lock)?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
    fs::hard_link(&target, &lock)?;
    assert!(matches!(
        ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &run, action),
        Err(LeaseError::InvalidLockFile)
    ));
    fs::remove_file(&lock)?;
    fs::create_dir(&lock)?;
    assert!(ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &run, action).is_err());
    fs::remove_dir(&lock)?;
    fs::write(&lock, b"persistent lock inode")?;
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600))?;
    let lease = ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &run, action)?;
    drop(lease);
    assert_eq!(fs::read(&lock)?, b"persistent lock inode");
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644))?;
    assert!(matches!(
        ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &run, action),
        Err(LeaseError::InvalidLockFile)
    ));
    let alias = root.0.join("alias");
    symlink(&root.0, &alias)?;
    assert!(ExecutionRoot::open(&alias).is_err());
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

/// The OS guard remains held through in-memory admission, completion, and verification.
#[test]
fn lease_remains_held_through_result_verification() -> Result<(), Box<dyn Error>> {
    let _process = PROCESS_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    use storage_engine::{
        AdmissionReport, BackendCompletion, BackendOutcome, LiveCheck, LiveCheckStatus,
        VerificationReport,
    };
    let root = Scratch::new()?;
    let (frozen, mut run) = fixture()?;
    let mut other = ExecutionRun::new(frozen.document())?;
    let action = ActionId::new(0);
    other.begin_check(action)?;
    let lease = ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &run, action)?;
    let instance = Uuid::now_v7();
    let report = AdmissionReport {
        run_id: lease.run_id(),
        action: lease.action(),
        observation: observation(&run, instance, 1),
        applicability: LiveCheckStatus::Satisfied,
        checks: checks(&run, LiveCheckStatus::Satisfied),
        held_resources: lease.held_resources().to_vec(),
    };
    assert!(run.record_admission(report)?);
    run.mark_dispatch_started(action)?;
    run.record_completion(BackendCompletion {
        run_id: run.run_id(),
        action,
        outcome: BackendOutcome::Succeeded,
        observation: observation(&run, instance, 2),
    })?;
    assert!(matches!(
        ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &other, action),
        Err(LeaseError::Busy)
    ));
    // These synthetic claims test guard lifetime; they do not establish native result evidence.
    let checks = run.context().plan.actions()[0]
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
        observation: run
            .completion(action)?
            .ok_or("completion")?
            .observation
            .reference,
        checks,
    })?;
    assert!(matches!(
        ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &other, action),
        Err(LeaseError::Busy)
    ));
    assert!(!run.is_verified());
    assert_eq!(fs::read_dir(&root.0)?.count(), 1);
    drop(lease);
    let next = ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &other, action)?;
    drop(next);
    Ok(())
}

/// Child entry point selected only by the parent test below; uses a test-owned temporary root.
#[test]
#[ignore = "subprocess helper invoked by lease_excludes_other_processes_and_survives_process_exit"]
fn lease_subprocess_child() -> Result<(), Box<dyn Error>> {
    let root = std::env::var_os("DISKSGRAPH_LEASE_TEST_ROOT").ok_or("missing subprocess root")?;
    let mode = std::env::var("DISKSGRAPH_LEASE_TEST_MODE")?;
    let (_, run) = fixture()?;
    let guard = ExecutionLease::try_acquire(
        &ExecutionRoot::open(PathBuf::from(root))?,
        &run,
        ActionId::new(0),
    );
    match mode.as_str() {
        "busy" => assert!(matches!(guard, Err(LeaseError::Busy))),
        "exit_holding" => {
            let _guard = guard?;
            // Bypass Rust destructors: the kernel, rather than RAII cleanup, releases this lock.
            std::process::exit(0);
        }
        _ => return Err("unknown subprocess mode".into()),
    }
    Ok(())
}

/// Actual child processes see contention and release the kernel lock even without Rust cleanup.
#[test]
fn lease_excludes_other_processes_and_survives_process_exit() -> Result<(), Box<dyn Error>> {
    let _process = PROCESS_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = Scratch::new()?;
    let (_, run) = fixture()?;
    let action = ActionId::new(0);
    let lease = ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &run, action)?;
    let spawn = |mode: &str| -> Result<(), Box<dyn Error>> {
        let result = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "lease_subprocess_child",
                "--ignored",
                "--nocapture",
            ])
            .env("DISKSGRAPH_LEASE_TEST_ROOT", &root.0)
            .env("DISKSGRAPH_LEASE_TEST_MODE", mode)
            .output()?;
        assert!(
            result.status.success(),
            "child {mode}: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(())
    };
    spawn("busy")?;
    drop(lease);
    spawn("exit_holding")?;
    let lease = ExecutionLease::try_acquire(&ExecutionRoot::open(&root.0)?, &run, action)?;
    drop(lease);
    Ok(())
}

/// Incident inspection and action execution share exclusion without needing a historical run.
#[test]
fn host_lock_and_action_lease_share_one_lock() -> Result<(), Box<dyn Error>> {
    let _process = PROCESS_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let scratch = Scratch::new()?;
    let root = ExecutionRoot::open(&scratch.0)?;
    let (_, run) = fixture()?;
    let guard = root.try_lock_host()?;
    assert!(matches!(root.try_lock_host(), Err(LeaseError::Busy)));
    assert!(matches!(
        ExecutionLease::try_acquire(&root, &run, ActionId::new(0)),
        Err(LeaseError::Busy)
    ));
    drop(guard);
    let lease = ExecutionLease::try_acquire(&root, &run, ActionId::new(0))?;
    assert!(matches!(root.try_lock_host(), Err(LeaseError::Busy)));
    drop(lease);
    let guard = root.try_lock_host()?;
    drop(guard);
    fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o777))?;
    assert!(matches!(
        root.try_lock_host(),
        Err(LeaseError::InsecureDirectory)
    ));
    Ok(())
}

/// Later path replacement cannot redirect operations through the previously checked directory fd.
#[test]
fn root_retains_the_opened_directory() -> Result<(), Box<dyn Error>> {
    let _process = PROCESS_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let scratch = Scratch::new()?;
    let path = scratch.0.join("root");
    fs::DirBuilder::new().mode(0o700).create(&path)?;
    let root = ExecutionRoot::open(&path)?;
    let retained = scratch.0.join("retained");
    fs::rename(&path, &retained)?;
    fs::DirBuilder::new().mode(0o700).create(&path)?;
    let guard = root.try_lock_host()?;
    assert!(retained.join("execution.lock").exists());
    assert!(!path.join("execution.lock").exists());
    drop(guard);
    Ok(())
}
