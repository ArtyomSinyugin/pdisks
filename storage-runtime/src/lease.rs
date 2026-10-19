//! Linux advisory exclusion for cooperating disksgraph hosts using one shared private directory.

use std::{io, path::Path};

use rustix::{
    fd::OwnedFd,
    fs::{AtFlags, CWD, FileType, FlockOperation, Mode, OFlags, flock, fstat, openat, statat},
    io::Errno,
    process::geteuid,
};
use storage_provider::ResourceAccess;
use thiserror::Error;
use uuid::Uuid;

use storage_engine::{ActionId, ActionRunState, ExecutionRun, ExecutionRunError};

/// Permanent name shared by every cooperating host, regardless of transient plan/node IDs.
const LOCK_FILE: &str = "execution.lock";

/// Failure to establish exclusion never permits dispatch.
#[derive(Debug, Error)]
pub enum LeaseError {
    /// Opening, inspecting, or locking the file failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Only a linked mode-0700 directory owned by the effective user is supported.
    #[error("lease directory must have mode 0700 and be owned by the effective user")]
    InsecureDirectory,
    /// Existing lock files must be regular, private, singly linked, and owned by this user.
    #[error("invalid execution lock file")]
    InvalidLockFile,
    /// Another cooperating host holds the global lease; acquisition never waits.
    #[error("another execution holds the host lease")]
    Busy,
    /// Acquisition is permitted only for the active Checking action.
    #[error("action must be Checking before acquiring its execution lease")]
    NotChecking,
    /// The requested action or run lifecycle is invalid.
    #[error(transparent)]
    Run(#[from] ExecutionRunError),
}

/// An existing private local directory beneath a trusted, stable parent.
/// Every cooperating process must use the same root; never replace it or its lock inode.
#[derive(Debug)]
pub struct ExecutionRoot {
    /// Opened once without following a final symlink; later operations are relative to this fd.
    directory: OwnedFd,
}

impl ExecutionRoot {
    /// Retains the same private root inode for a synchronous coordinator without reopening its pathname.
    pub fn try_clone(&self) -> Result<Self, LeaseError> {
        Ok(Self {
            directory: self.directory.try_clone()?,
        })
    }

    /// Opens a caller-configured mode-0700 directory and rejects unlinked or foreign-owned roots.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LeaseError> {
        let directory = openat(
            CWD,
            path.as_ref(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        validate_directory(&directory)?;
        Ok(Self { directory })
    }

    /// Acquires the shared nonblocking lock without reconstructing any execution run.
    pub fn try_lock_host(&self) -> Result<HostExecutionLock, LeaseError> {
        validate_directory(&self.directory)?;
        let lock = openat(
            &self.directory,
            LOCK_FILE,
            OFlags::CREATE | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(io::Error::from)?;
        validate_lock(&self.directory, &lock)?;
        match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(Errno::WOULDBLOCK) => return Err(LeaseError::Busy),
            Err(error) => return Err(io::Error::from(error).into()),
        }
        // Catch replacement or permission changes while acquisition was in progress.
        validate_lock(&self.directory, &lock)?;
        let host_lock = HostExecutionLock {
            directory: self.directory.try_clone()?,
            _lock: lock,
        };
        validate_directory(host_lock.directory())?;
        Ok(host_lock)
    }
}

/// Enforces the documented directory policy again on retained descriptors before use.
fn validate_directory(directory: &OwnedFd) -> Result<(), LeaseError> {
    let info = fstat(directory).map_err(io::Error::from)?;
    if info.st_uid != geteuid().as_raw() || info.st_mode & 0o7777 != 0o700 || info.st_nlink == 0 {
        return Err(LeaseError::InsecureDirectory);
    }
    Ok(())
}

/// Ensures the held descriptor still names the private permanent lock in this root.
fn validate_lock(directory: &OwnedFd, lock: &OwnedFd) -> Result<(), LeaseError> {
    let info = fstat(lock).map_err(io::Error::from)?;
    let named = statat(directory, LOCK_FILE, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    if FileType::from_raw_mode(info.st_mode) != FileType::RegularFile
        || info.st_uid != geteuid().as_raw()
        || info.st_mode & 0o077 != 0
        || info.st_nlink != 1
        || info.st_dev != named.st_dev
        || info.st_ino != named.st_ino
    {
        return Err(LeaseError::InvalidLockFile);
    }
    Ok(())
}

/// The single OS lock used for both execution and incident inspection.
/// Dropping the guard releases exclusion, including on process termination.
#[derive(Debug)]
pub struct HostExecutionLock {
    /// Retained root for descriptor-relative checkpoint operations while this guard is borrowed.
    directory: OwnedFd,
    /// Never exposed; internal lease duplicates retain the same flock while the run guard lives.
    _lock: OwnedFd,
}

impl HostExecutionLock {
    /// Borrows the retained root descriptor without exposing or cloning the lock itself.
    pub(crate) fn directory(&self) -> &OwnedFd {
        &self.directory
    }
}

/// Exclusive cooperative lease bound to one run/action and its trusted resource declarations.
/// Retain it through verification; arbitrary external disk tools do not participate in this lock.
#[derive(Debug)]
pub struct ExecutionLease {
    /// The same lock used by incident inspection, retained through action verification.
    host_lock: HostExecutionLock,
    /// Attempt that acquired this guard.
    run_id: Uuid,
    /// Active action covered by the global lease.
    action: ActionId,
    /// Logical resources from the trusted frozen contract.
    resources: Vec<ResourceAccess>,
}

impl ExecutionLease {
    /// Binds an action to an already held run lock without releasing exclusion between DAG actions.
    pub(crate) fn under_host_lock(
        lock: &HostExecutionLock,
        run: &ExecutionRun,
        action: ActionId,
    ) -> Result<Self, LeaseError> {
        Self::from_lock(
            HostExecutionLock {
                directory: lock.directory.try_clone()?,
                _lock: lock._lock.try_clone()?,
            },
            run,
            action,
        )
    }

    /// Acquires OS exclusion before the host obtains its admission probe.
    pub fn try_acquire(
        root: &ExecutionRoot,
        run: &ExecutionRun,
        action: ActionId,
    ) -> Result<Self, LeaseError> {
        if run.state(action)? != ActionRunState::Checking {
            return Err(LeaseError::NotChecking);
        }
        let host_lock = root.try_lock_host()?;
        Self::from_lock(host_lock, run, action)
    }

    /// Copies trusted resource declarations while retaining ownership of the same OS exclusion.
    fn from_lock(
        host_lock: HostExecutionLock,
        run: &ExecutionRun,
        action: ActionId,
    ) -> Result<Self, LeaseError> {
        if run.state(action)? != ActionRunState::Checking {
            return Err(LeaseError::NotChecking);
        }
        let resources = run
            .context()
            .plan
            .actions()
            .iter()
            .find(|candidate| candidate.id == action)
            .ok_or(ExecutionRunError::UnknownAction(action))?
            .informational_contract
            .affected_resources
            .clone();
        Ok(Self {
            host_lock,
            run_id: run.run_id(),
            action,
            resources,
        })
    }

    /// Borrows the held host lock for operations that must not outlive OS exclusion.
    pub fn host_lock(&self) -> &HostExecutionLock {
        &self.host_lock
    }

    /// Returns the attempt bound at acquisition; a new run requires a new guard.
    pub const fn run_id(&self) -> Uuid {
        self.run_id
    }

    /// Returns the sole action covered by this guard.
    pub const fn action(&self) -> ActionId {
        self.action
    }

    /// Describes logical resources covered by the stronger global cooperative exclusion.
    pub fn held_resources(&self) -> &[ResourceAccess] {
        &self.resources
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, PermissionsExt},
    };

    /// Replacing a permanent name cannot make an old descriptor certify the new lock domain.
    #[test]
    fn lock_validation_rejects_replacement_and_permission_changes()
    -> Result<(), Box<dyn std::error::Error>> {
        let path =
            std::env::temp_dir().join(format!("disksgraph-lock-identity-{}", Uuid::now_v7()));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        let root = ExecutionRoot::open(&path)?;
        let guard = root.try_lock_host()?;
        let lock = path.join(LOCK_FILE);
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644))?;
        assert!(matches!(
            validate_lock(&root.directory, &guard._lock),
            Err(LeaseError::InvalidLockFile)
        ));
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o600))?;
        validate_lock(&root.directory, &guard._lock)?;
        fs::rename(&lock, path.join("old.lock"))?;
        let replacement = root.try_lock_host()?;
        assert!(matches!(
            validate_lock(&root.directory, &guard._lock),
            Err(LeaseError::InvalidLockFile)
        ));
        drop(replacement);
        fs::remove_file(&lock)?;
        std::os::unix::fs::symlink(path.join("old.lock"), &lock)?;
        assert!(matches!(
            validate_lock(&root.directory, &guard._lock),
            Err(LeaseError::InvalidLockFile)
        ));
        drop(guard);
        drop(root);
        fs::remove_dir_all(path)?;
        Ok(())
    }
}
