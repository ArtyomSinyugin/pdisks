//! Descriptor-relative Linux publication and removal under the shared host lock.

use rustix::{
    fs::{
        AtFlags, FileType, Mode, OFlags, RenameFlags, Stat, fchmod, fstat, fsync, openat,
        renameat_with, statat, unlinkat,
    },
    io::Errno,
    process::geteuid,
};
use std::{
    cell::Cell,
    fs::File,
    io::{self, Read, Write},
};
use storage_engine::ActionId;
use uuid::Uuid;

use super::{ActiveAttempt, CheckpointError, MAX_CHECKPOINT_BYTES};
use crate::lease::HostExecutionLock;

/// Sole active checkpoint name; no sequence of historical files is produced.
const ACTIVE_FILE: &str = "active-attempt.json";

/// One active checkpoint, usable only while its borrowed host lock remains held.
/// Any mutation I/O error forbids further mutations through this handle; stop the run.
/// Unsupported no-replace rename or directory fsync fails closed, without a weaker fallback.
#[derive(Debug)]
pub struct CheckpointStore<'lock> {
    /// Prevents release of OS exclusion while file operations are in progress.
    lock: &'lock HostExecutionLock,
    /// Set after an unsuccessful mutation, including ambiguous sync acknowledgments.
    poisoned: Cell<bool>,
}

impl<'lock> CheckpointStore<'lock> {
    /// Binds all file operations to the directory of the already-held shared OS lock.
    pub fn new(lock: &'lock HostExecutionLock) -> Self {
        Self {
            lock,
            poisoned: Cell::new(false),
        }
    }

    /// Reads a bounded, private regular file; only a missing name counts as no incident.
    /// Corruption, unsupported versions, symlinks, and hard links block new execution.
    pub fn inspect(&self) -> Result<Option<ActiveAttempt>, CheckpointError> {
        Ok(self.read_current()?.map(|(attempt, _)| attempt))
    }

    /// Publishes one fully synchronized checkpoint without ever replacing an active attempt.
    /// The caller may cross the invocation boundary only after this method returns success.
    pub fn arm(&self, attempt: &ActiveAttempt) -> Result<(), CheckpointError> {
        self.require_healthy()?;
        if self.inspect()?.is_some() {
            return Err(CheckpointError::AlreadyActive);
        }
        let bytes = attempt.to_bytes()?;
        let directory = self.lock.directory();
        let temporary = format!(".active-attempt-{}.tmp", Uuid::now_v7());
        let fd = openat(
            directory,
            &temporary,
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|error| {
            self.poisoned.set(true);
            CheckpointError::Io(error.into())
        })?;
        let result = (|| {
            // Set exact private permissions even under an unusually restrictive caller umask.
            fchmod(&fd, Mode::RUSR | Mode::WUSR).map_err(io::Error::from)?;
            let mut file = File::from(fd);
            #[cfg(test)]
            {
                // Exercise cleanup after a real short write, before the remaining bytes reach disk.
                file.write_all(&bytes[..bytes.len() / 2])?;
                tests::boundary(tests::Boundary::Write)?;
                file.write_all(&bytes[bytes.len() / 2..])?;
            }
            #[cfg(not(test))]
            file.write_all(&bytes)?;
            #[cfg(test)]
            tests::boundary(tests::Boundary::FileSync)?;
            file.sync_all()?;
            #[cfg(test)]
            tests::boundary(tests::Boundary::Publish)?;
            match renameat_with(
                directory,
                &temporary,
                directory,
                ACTIVE_FILE,
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => {}
                Err(Errno::EXIST) => return Err(CheckpointError::AlreadyActive),
                Err(error) => return Err(io::Error::from(error).into()),
            }
            #[cfg(test)]
            tests::boundary(tests::Boundary::DirectorySync)?;
            fsync(directory).map_err(io::Error::from)?;
            Ok(())
        })();
        if result.is_err() {
            // Only our unique staging name is cleaned; a published active attempt always survives.
            let _ = unlinkat(directory, &temporary, AtFlags::empty());
            self.poisoned.set(true);
        }
        result
    }

    /// Removes only this run/action's exact checkpoint and confirms the removal with directory fsync.
    /// A clear error forbids starting a successor, even when the action was already verified.
    pub fn clear(&self, run: Uuid, action: ActionId) -> Result<(), CheckpointError> {
        self.require_healthy()?;
        let Some((attempt, original)) = self.read_current()? else {
            return Err(CheckpointError::AttemptMismatch);
        };
        if attempt.run_id != run || attempt.action_id != action {
            return Err(CheckpointError::AttemptMismatch);
        }
        let directory = self.lock.directory();
        let current =
            statat(directory, ACTIVE_FILE, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
        validate_file(&current)?;
        if original.st_dev != current.st_dev || original.st_ino != current.st_ino {
            return Err(CheckpointError::InvalidFile);
        }
        let result = (|| {
            #[cfg(test)]
            tests::boundary(tests::Boundary::Unlink)?;
            unlinkat(directory, ACTIVE_FILE, AtFlags::empty()).map_err(io::Error::from)?;
            #[cfg(test)]
            tests::boundary(tests::Boundary::ClearSync)?;
            fsync(directory).map_err(io::Error::from)?;
            Ok(())
        })();
        if result.is_err() {
            self.poisoned.set(true);
        }
        result
    }

    /// Retains the opened file identity for the final no-follow check before removal.
    fn read_current(&self) -> Result<Option<(ActiveAttempt, Stat)>, CheckpointError> {
        #[cfg(test)]
        tests::boundary(tests::Boundary::Read)?;
        let fd = match openat(
            self.lock.directory(),
            ACTIVE_FILE,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(io::Error::from(error).into()),
        };
        let info = fstat(&fd).map_err(io::Error::from)?;
        validate_file(&info)?;
        if info.st_size > MAX_CHECKPOINT_BYTES as i64 {
            return Err(CheckpointError::TooLarge);
        }
        let mut bytes = Vec::new();
        File::from(fd)
            .take(MAX_CHECKPOINT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(Some((ActiveAttempt::from_bytes(&bytes)?, info)))
    }

    /// An uncertain mutation must never be followed by another action on the same store handle.
    fn require_healthy(&self) -> Result<(), CheckpointError> {
        if self.poisoned.get() {
            Err(CheckpointError::Poisoned)
        } else {
            Ok(())
        }
    }
}

/// Rejects objects whose metadata would permit ambiguity or access by another user.
fn validate_file(info: &Stat) -> Result<(), CheckpointError> {
    if FileType::from_raw_mode(info.st_mode) != FileType::RegularFile
        || info.st_uid != geteuid().as_raw()
        || info.st_mode & 0o077 != 0
        || info.st_nlink != 1
    {
        return Err(CheckpointError::InvalidFile);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
