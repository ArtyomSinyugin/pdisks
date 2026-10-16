//! Linux advisory exclusion for cooperating disksgraph hosts using one shared private directory.

use std::{io, path::Path};

use rustix::{
    fd::OwnedFd,
    fs::{CWD, FileType, FlockOperation, Mode, OFlags, flock, fstat, openat},
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
    /// Only an existing private directory owned by the effective user is supported.
    #[error("lease directory must be private and owned by the effective user")]
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
    /// Opens a caller-configured directory and rejects public or foreign-owned roots.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LeaseError> {
        let directory = openat(
            CWD,
            path.as_ref(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let info = fstat(&directory).map_err(io::Error::from)?;
        if info.st_uid != geteuid().as_raw() || info.st_mode & 0o077 != 0 {
            return Err(LeaseError::InsecureDirectory);
        }
        Ok(Self { directory })
    }

    /// Acquires the shared nonblocking lock without reconstructing any execution run.
    pub fn try_lock_host(&self) -> Result<HostExecutionLock, LeaseError> {
        let lock = openat(
            &self.directory,
            LOCK_FILE,
            OFlags::CREATE | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(io::Error::from)?;
        let info = fstat(&lock).map_err(io::Error::from)?;
        if FileType::from_raw_mode(info.st_mode) != FileType::RegularFile
            || info.st_uid != geteuid().as_raw()
            || info.st_mode & 0o077 != 0
            || info.st_nlink != 1
        {
            return Err(LeaseError::InvalidLockFile);
        }
        match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(Errno::WOULDBLOCK) => return Err(LeaseError::Busy),
            Err(error) => return Err(io::Error::from(error).into()),
        }
        let host_lock = HostExecutionLock {
            directory: self.directory.try_clone()?,
            _lock: lock,
        };
        let info = fstat(host_lock.directory()).map_err(io::Error::from)?;
        if info.st_uid != geteuid().as_raw() || info.st_mode & 0o077 != 0 {
            return Err(LeaseError::InsecureDirectory);
        }
        Ok(host_lock)
    }
}

/// The single OS lock used for both execution and incident inspection.
/// Dropping the guard releases exclusion, including on process termination.
#[derive(Debug)]
pub struct HostExecutionLock {
    /// Retained root for descriptor-relative checkpoint operations while this guard is borrowed.
    directory: OwnedFd,
    /// Never cloned or exposed: close releases flock.
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
