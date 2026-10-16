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
