//! Linux advisory exclusion for cooperating pdisks hosts using one shared private directory.

use std::{io, path::Path};

use rustix::{
    fd::OwnedFd,
    fs::{CWD, FileType, FlockOperation, Mode, OFlags, flock, fstat, openat},
    io::Errno,
    process::geteuid,
};
use storage_provider::{ActionPrerequisite, ResourceAccess};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    ActionId, ActionRunState, AdmissionReport, ExecutionRun, ExecutionRunError, LiveCheck,
    LiveCheckStatus, LiveObservation,
};

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
    /// A guard acquired for one attempt cannot attest another attempt over the same plan.
    #[error("execution lease belongs to another run")]
    RunMismatch,
    /// The requested action or run lifecycle is invalid.
    #[error(transparent)]
    Run(#[from] ExecutionRunError),
}

/// Exclusive cooperative lease bound to one run/action; dropping it releases the OS lock.
/// All hosts must share a stable local directory and keep this guard through verification.
/// This serializes every resource conservatively; it cannot exclude arbitrary external tools.
#[derive(Debug)]
pub struct ExecutionLease {
    /// Retains the opened directory without relying on later path resolution.
    _directory: OwnedFd,
    /// Never cloned or exposed: close releases flock, including after process termination.
    _lock: OwnedFd,
    /// Attempt that acquired this guard.
    run_id: Uuid,
    /// Active action whose entire resource set is covered by the global lease.
    action: ActionId,
    /// Logical resource declarations from the trusted frozen contract.
    resources: Vec<ResourceAccess>,
}

impl ExecutionLease {
    /// Acquires a nonblocking OS lease before the host obtains its admission probe.
    /// The caller supplies an existing private local directory under a trusted stable parent.
    /// Never remove/replace this directory or its lock file while hosts may be running.
    pub fn try_acquire(
        directory: impl AsRef<Path>,
        run: &ExecutionRun,
        action: ActionId,
    ) -> Result<Self, LeaseError> {
        if run.state(action)? != ActionRunState::Checking {
            return Err(LeaseError::NotChecking);
        }
        let directory = openat(
            CWD,
            directory.as_ref(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let info = fstat(&directory).map_err(io::Error::from)?;
        if info.st_uid != geteuid().as_raw() || info.st_mode & 0o077 != 0 {
            return Err(LeaseError::InsecureDirectory);
        }
        let lock = openat(
            &directory,
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
            _directory: directory,
            _lock: lock,
            run_id: run.run_id(),
            action,
            resources,
        })
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

    /// Records host-supplied evidence with resources covered by this still-held OS guard.
    /// The host must probe after acquisition and retain the guard through result verification.
    /// This supplies exclusion only; the host remains responsible for evidence and applicability.
    pub fn record_admission(
        &self,
        run: &mut ExecutionRun,
        observation: LiveObservation,
        applicability: LiveCheckStatus,
        checks: Vec<LiveCheck<ActionPrerequisite>>,
    ) -> Result<bool, LeaseError> {
        self.check_run(run)?;
        Ok(run.record_admission(AdmissionReport {
            run_id: self.run_id,
            action: self.action,
            observation,
            applicability,
            held_resources: self.resources.clone(),
            checks,
        })?)
    }

    /// Rejects accidental reuse of a lease with another attempt before changing any run state.
    fn check_run(&self, run: &ExecutionRun) -> Result<(), LeaseError> {
        if run.run_id() != self.run_id {
            return Err(LeaseError::RunMismatch);
        }
        Ok(())
    }
}
