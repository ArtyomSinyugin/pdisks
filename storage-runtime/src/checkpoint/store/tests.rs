//! Fault and crash tests touch private temporary files and never call storage backends.

use super::*;
use crate::{
    checkpoint::{CHECKPOINT_SCHEMA_VERSION, TargetFingerprint},
    lease::{ExecutionRoot, LeaseError},
};
use std::{
    cell::Cell,
    error::Error,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};
use storage_core::model::{Bytes, ExternalId, LvmLvKind, NodeId};
use storage_provider::{BackendId, LvmAction, ProviderAction};

/// Fork briefly inherits other threads' CLOEXEC fds; isolate immediate lock-release assertions.
static PROCESS_TESTS: Mutex<()> = Mutex::new(());

/// Serializes these file tests with their SIGKILL subprocess test, not production host operations.
fn serialize_process_tests() -> MutexGuard<'static, ()> {
    PROCESS_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Boundaries where a failed acknowledgment or process death must prevent ordinary continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Boundary {
    /// A staging file contains only the first half of its serialized attempt.
    Write,
    /// Contents have been written, but file fsync has not yet succeeded.
    FileSync,
    /// Contents are synchronized, but the active name has not been published.
    Publish,
    /// Active name is visible, but directory fsync has not yet succeeded.
    DirectorySync,
    /// The owned active file has been checked, but unlink has not succeeded.
    Unlink,
    /// Active name was removed, but removal has not yet been synchronized.
    ClearSync,
}

thread_local! {
    /// Thread-scoped faults cannot affect a simultaneously executing unrelated test.
    static FAULT: Cell<Option<(Boundary, bool)>> = const { Cell::new(None) };
}

/// Injects a single I/O failure or SIGKILL strictly within test builds.
pub(super) fn boundary(point: Boundary) -> io::Result<()> {
    if let Some((expected, crash)) = FAULT.get()
        && expected == point
    {
        FAULT.set(None);
        if crash {
            rustix::process::kill_process(
                rustix::process::getpid(),
                rustix::process::Signal::KILL,
            )?;
            unreachable!("SIGKILL must terminate the test child");
        }
        return Err(io::Error::other("injected checkpoint I/O failure"));
    }
    Ok(())
}

/// Owns only a newly allocated test directory, removed after all handles have dropped.
struct Scratch(PathBuf);
impl Scratch {
    /// Creates private state under the OS-provided temporary directory.
    fn new() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!("disksgraph-checkpoint-{}", Uuid::now_v7()));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    /// Production code never deletes execution roots or persistent lock inodes.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Synthetic diagnostic data tests file semantics, not real admission or target resolution.
fn fixture() -> ActiveAttempt {
    let vg = NodeId::new();
    ActiveAttempt {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        run_id: Uuid::now_v7(),
        plan_id: Uuid::now_v7(),
        action_id: ActionId::new(0),
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
            external_ids: vec![ExternalId::LvmVgUuid("test-vg-uuid".into())],
        }],
        plan_digest: "a".repeat(64),
        backend_id: BackendId::new("libblockdev-lvm").unwrap_or_else(|| unreachable!()),
    }
}

/// The sole active file cannot be overwritten or cleared using another run/action identity.
#[test]
fn arm_and_clear_preserve_exact_attempt_ownership() -> Result<(), Box<dyn Error>> {
    let _serial = serialize_process_tests();
    let scratch = Scratch::new()?;
    let root = ExecutionRoot::open(&scratch.0)?;
    let lock = root.try_lock_host()?;
    let store = CheckpointStore::new(&lock);
    assert!(store.inspect()?.is_none());
    let attempt = fixture();
    store.arm(&attempt)?;
    let active = scratch.0.join(ACTIVE_FILE);
    let bytes = fs::read(&active)?;
    assert_eq!(fs::metadata(&active)?.permissions().mode() & 0o777, 0o600);
    assert_eq!(store.inspect()?, Some(attempt.clone()));
    assert!(matches!(
        store.arm(&fixture()),
        Err(CheckpointError::AlreadyActive)
    ));
    assert!(matches!(
        store.clear(Uuid::now_v7(), attempt.action_id),
        Err(CheckpointError::AttemptMismatch)
    ));
    assert!(matches!(
        store.clear(attempt.run_id, ActionId::new(1)),
        Err(CheckpointError::AttemptMismatch)
    ));
    assert_eq!(fs::read(&active)?, bytes);
    assert_eq!(fs::read_dir(&scratch.0)?.count(), 2);
    assert!(matches!(root.try_lock_host(), Err(LeaseError::Busy)));
    store.clear(attempt.run_id, attempt.action_id)?;
    assert!(store.inspect()?.is_none());
    assert_eq!(fs::read_dir(&scratch.0)?.count(), 1);
    assert!(matches!(
        store.clear(attempt.run_id, attempt.action_id),
        Err(CheckpointError::AttemptMismatch)
    ));
    store.arm(&fixture())?;
    Ok(())
}

/// Releasing the OS guard does not remove evidence; a fresh process/lock sees the same incident.
#[test]
fn checkpoint_survives_lock_release() -> Result<(), Box<dyn Error>> {
    let _serial = serialize_process_tests();
    let scratch = Scratch::new()?;
    let root = ExecutionRoot::open(&scratch.0)?;
    let attempt = fixture();
    {
        let lock = root.try_lock_host()?;
        CheckpointStore::new(&lock).arm(&attempt)?;
    }
    let lock = ExecutionRoot::open(&scratch.0)?.try_lock_host()?;
    let store = CheckpointStore::new(&lock);
    assert_eq!(store.inspect()?, Some(attempt.clone()));
    assert!(matches!(
        store.arm(&fixture()),
        Err(CheckpointError::AlreadyActive)
    ));
    store.clear(attempt.run_id, attempt.action_id)?;
    Ok(())
}

/// Unknown or unsafe existing names block all mutations and leave their data untouched.
#[test]
fn malformed_and_untrusted_files_fail_closed() -> Result<(), Box<dyn Error>> {
    let _serial = serialize_process_tests();
    for case in 0..7 {
        let scratch = Scratch::new()?;
        let root = ExecutionRoot::open(&scratch.0)?;
        let lock = root.try_lock_host()?;
        let store = CheckpointStore::new(&lock);
        let active = scratch.0.join(ACTIVE_FILE);
        match case {
            0 => fs::write(&active, b"{broken")?,
            1 => fs::write(&active, vec![b' '; MAX_CHECKPOINT_BYTES + 1])?,
            2 => {
                fs::write(&active, fixture().to_bytes()?)?;
                fs::set_permissions(&active, fs::Permissions::from_mode(0o644))?;
            }
            3 | 4 => {
                let other = scratch.0.join("untouched");
                fs::write(&other, fixture().to_bytes()?)?;
                fs::set_permissions(&other, fs::Permissions::from_mode(0o600))?;
                if case == 3 {
                    std::os::unix::fs::symlink(&other, &active)?;
                } else {
                    fs::hard_link(&other, &active)?;
                }
            }
            5 => fs::create_dir(&active)?,
            _ => rustix::fs::mkfifoat(rustix::fs::CWD, &active, Mode::RUSR | Mode::WUSR)?,
        }
        if case < 2 {
            fs::set_permissions(&active, fs::Permissions::from_mode(0o600))?;
        }
        assert!(store.inspect().is_err(), "inspect case {case}");
        assert!(store.arm(&fixture()).is_err(), "arm case {case}");
        assert!(
            store.clear(Uuid::now_v7(), ActionId::new(0)).is_err(),
            "clear case {case}"
        );
        assert!(active.symlink_metadata().is_ok());
        assert_eq!(
            fs::read_dir(&scratch.0)?.count(),
            if case == 3 || case == 4 { 3 } else { 2 }
        );
    }
    Ok(())
}

/// Failed prepare cannot dispatch; a visible but unsynced checkpoint remains for inspection.
#[test]
fn arm_io_failures_stop_dispatch_and_poison_mutations() -> Result<(), Box<dyn Error>> {
    let _serial = serialize_process_tests();
    for point in [
        Boundary::Write,
        Boundary::FileSync,
        Boundary::Publish,
        Boundary::DirectorySync,
    ] {
        let scratch = Scratch::new()?;
        let root = ExecutionRoot::open(&scratch.0)?;
        let lock = root.try_lock_host()?;
        let store = CheckpointStore::new(&lock);
        let attempt = fixture();
        FAULT.set(Some((point, false)));
        assert!(store.arm(&attempt).is_err());
        assert!(matches!(
            store.arm(&attempt),
            Err(CheckpointError::Poisoned)
        ));
        assert!(matches!(
            store.clear(attempt.run_id, attempt.action_id),
            Err(CheckpointError::Poisoned)
        ));
        assert_eq!(store.inspect()?.is_some(), point == Boundary::DirectorySync);
        assert_eq!(
            fs::read_dir(&scratch.0)?.count(),
            if point == Boundary::DirectorySync {
                2
            } else {
                1
            }
        );
    }
    Ok(())
}

/// Unacknowledged removal prevents a successor even when the active name is already absent.
#[test]
fn clear_sync_failure_stops_further_mutations() -> Result<(), Box<dyn Error>> {
    let _serial = serialize_process_tests();
    let scratch = Scratch::new()?;
    let root = ExecutionRoot::open(&scratch.0)?;
    let lock = root.try_lock_host()?;
    let store = CheckpointStore::new(&lock);
    let attempt = fixture();
    store.arm(&attempt)?;
    FAULT.set(Some((Boundary::ClearSync, false)));
    assert!(store.clear(attempt.run_id, attempt.action_id).is_err());
    assert!(store.inspect()?.is_none());
    assert!(matches!(
        store.arm(&fixture()),
        Err(CheckpointError::Poisoned)
    ));
    Ok(())
}

/// Child entry is invoked only by the crash test with a private root created by its parent.
#[test]
#[ignore = "subprocess helper selected by sigkill_preserves_conservative_checkpoint_state"]
fn checkpoint_crash_child() -> Result<(), Box<dyn Error>> {
    let path = std::env::var_os("DISKSGRAPH_CHECKPOINT_TEST_ROOT").ok_or("missing child root")?;
    let mode = std::env::var("DISKSGRAPH_CHECKPOINT_TEST_MODE")?;
    let root = ExecutionRoot::open(PathBuf::from(path))?;
    let lock = root.try_lock_host()?;
    let store = CheckpointStore::new(&lock);
    let point = match mode.as_str() {
        "before_publish" => Boundary::Publish,
        "after_publish" => Boundary::DirectorySync,
        "after_clear" => Boundary::ClearSync,
        _ => return Err("invalid crash mode".into()),
    };
    if point == Boundary::ClearSync {
        store.arm(&fixture())?;
    }
    FAULT.set(Some((point, true)));
    if point == Boundary::ClearSync {
        let attempt = store.inspect()?.ok_or("active attempt")?;
        store.clear(attempt.run_id, attempt.action_id)?;
    } else {
        store.arm(&fixture())?;
    }
    Err("crash boundary did not kill child".into())
}

/// SIGKILL releases flock; before publication there is no active marker, afterwards writes block.
#[test]
fn sigkill_preserves_conservative_checkpoint_state() -> Result<(), Box<dyn Error>> {
    let _serial = serialize_process_tests();
    use std::os::unix::process::ExitStatusExt;
    for mode in ["before_publish", "after_publish", "after_clear"] {
        let scratch = Scratch::new()?;
        let result = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "checkpoint::store::tests::checkpoint_crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env("DISKSGRAPH_CHECKPOINT_TEST_ROOT", &scratch.0)
            .env("DISKSGRAPH_CHECKPOINT_TEST_MODE", mode)
            .output()?;
        assert_eq!(
            result.status.signal(),
            Some(9),
            "{mode}: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        let root = ExecutionRoot::open(&scratch.0)?;
        let lock = root.try_lock_host()?;
        let store = CheckpointStore::new(&lock);
        if mode == "after_publish" {
            assert!(store.inspect()?.is_some());
            assert!(matches!(
                store.arm(&fixture()),
                Err(CheckpointError::AlreadyActive)
            ));
        } else {
            assert!(store.inspect()?.is_none());
        }
        if mode == "before_publish" {
            // An unpublished staging file is inert; no backend could have been invoked.
            assert_eq!(fs::read_dir(&scratch.0)?.count(), 2);
        }
    }
    Ok(())
}

/// A failed unlink retains exact evidence and poisons the handle even after verified execution.
#[test]
fn unlink_failure_retains_checkpoint_and_blocks_successor() -> Result<(), Box<dyn Error>> {
    let _serial = serialize_process_tests();
    let scratch = Scratch::new()?;
    let root = ExecutionRoot::open(&scratch.0)?;
    let lock = root.try_lock_host()?;
    let store = CheckpointStore::new(&lock);
    let attempt = fixture();
    store.arm(&attempt)?;
    let bytes = fs::read(scratch.0.join(ACTIVE_FILE))?;
    FAULT.set(Some((Boundary::Unlink, false)));
    assert!(matches!(
        store.clear(attempt.run_id, attempt.action_id),
        Err(CheckpointError::Io(_))
    ));
    assert_eq!(fs::read(scratch.0.join(ACTIVE_FILE))?, bytes);
    assert_eq!(store.inspect()?, Some(attempt.clone()));
    assert!(matches!(
        store.clear(attempt.run_id, attempt.action_id),
        Err(CheckpointError::Poisoned)
    ));
    assert!(matches!(
        store.arm(&fixture()),
        Err(CheckpointError::Poisoned)
    ));
    // A fresh handle still sees the incident; poisoning is not the persistence mechanism.
    assert!(matches!(
        CheckpointStore::new(&lock).arm(&fixture()),
        Err(CheckpointError::AlreadyActive)
    ));
    Ok(())
}
