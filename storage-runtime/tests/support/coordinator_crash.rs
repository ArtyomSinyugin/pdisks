//! Real coordinator crash boundaries over the existing recorded host and synthetic dispatcher.

use super::*;
use std::{
    fs,
    os::unix::{fs::DirBuilderExt, process::ExitStatusExt},
    path::PathBuf,
};
use storage_engine::ActionRunState;
use storage_runtime::{
    checkpoint::{ActiveAttempt, CheckpointStore},
    coordinator::ExecutionCoordinator,
    lease::ExecutionRoot,
};

/// Runs one isolated child and retains its root plus parent-readable pre-crash evidence.
fn kill_at(phase: &str) -> Result<admission::Scratch, Box<dyn Error>> {
    let scratch = admission::Scratch::new()?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(scratch.0.join("execution"))?;
    let result = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "coordinator_crash::coordinator_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("DISKSGRAPH_COORDINATOR_ROOT", &scratch.0)
        .env("DISKSGRAPH_COORDINATOR_PHASE", phase)
        .output()?;
    assert_eq!(
        result.status.signal(),
        Some(9),
        "{phase}: {}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(scratch)
}

/// Selected by the parent only; reports and graph effects come from the existing fake adapter.
#[test]
#[ignore = "subprocess helper invoked by coordinator SIGKILL tests"]
fn coordinator_crash_child() -> Result<(), Box<dyn Error>> {
    let parent = PathBuf::from(std::env::var_os("DISKSGRAPH_COORDINATOR_ROOT").ok_or("root")?);
    let phase = std::env::var("DISKSGRAPH_COORDINATOR_PHASE")?;
    let scratch = admission::Scratch(parent.join("execution"));
    let (state, frozen) = fixture()?;
    let state = Rc::new(RefCell::new(state));
    let mut run = ExecutionRun::new(frozen.document())?;
    let mut fake = coordination::dispatcher(&scratch, state.clone());
    fake.created_uuid = Some("created-lv".into());
    let calls = fake.calls.clone();
    let mut coordinator = ExecutionCoordinator::new(
        ExecutionRoot::open(&scratch.0)?,
        coordination::host(state, &run)?,
        fake,
        &run,
    );
    coordinator.execute_run_with_updates(&mut run, &mut |run| {
        let action = ActionId::new(0);
        let state = run
            .state(action)
            .unwrap_or_else(|error| panic!("state: {error}"));
        let target = match phase.as_str() {
            "armed" => state == ActionRunState::Admitted && calls.get() == 0,
            "running" => state == ActionRunState::Running && calls.get() == 1,
            "completed" => state == ActionRunState::Verifying && calls.get() == 1,
            "verified_before_clear" => state == ActionRunState::Verified && calls.get() == 1,
            _ => panic!("unknown crash phase"),
        };
        let active = scratch.0.join("active-attempt.json");
        if target && active.exists() {
            assert!(!run.is_verified());
            let bytes = fs::read(&active).unwrap_or_else(|error| panic!("marker: {error}"));
            let attempt = ActiveAttempt::from_bytes(&bytes)
                .unwrap_or_else(|error| panic!("attempt: {error}"));
            assert_eq!(attempt.run_id, run.run_id());
            assert_eq!(attempt.action_id, action);
            fs::write(parent.join("expected.json"), bytes)
                .unwrap_or_else(|error| panic!("evidence: {error}"));
            fs::write(parent.join("calls"), calls.get().to_string())
                .unwrap_or_else(|error| panic!("counter: {error}"));
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL)
                .unwrap_or_else(|error| panic!("kill: {error}"));
            unreachable!("SIGKILL must terminate child");
        }
    })?;
    Err("coordinator did not reach crash boundary".into())
}

/// SIGKILL preserves the exact armed attempt and kernel exclusion is released in all four phases.
#[test]
fn coordinator_sigkill_preserves_checkpoint_at_durable_boundaries() -> Result<(), Box<dyn Error>> {
    let _process = isolate_processes();
    for phase in ["armed", "running", "completed", "verified_before_clear"] {
        let scratch = kill_at(phase)?;
        let path = scratch.0.join("execution");
        let expected = fs::read(scratch.0.join("expected.json"))?;
        assert_eq!(fs::read(path.join("active-attempt.json"))?, expected);
        assert_eq!(
            fs::read_to_string(scratch.0.join("calls"))?,
            if phase == "armed" { "0" } else { "1" }
        );
        let root = ExecutionRoot::open(&path)?;
        let lock = root.try_lock_host()?;
        assert_eq!(
            CheckpointStore::new(&lock).inspect()?,
            Some(ActiveAttempt::from_bytes(&expected)?)
        );
        assert_eq!(fs::read_dir(&path)?.count(), 2);
    }
    Ok(())
}

/// A pending marker must be rejected before any observation or admission request reaches the host.
struct NoProbe;
impl storage_runtime::host::RuntimeHost for NoProbe {
    /// Entering a provider pass while a crash marker exists would violate the restart gate.
    fn probe(&mut self) -> Result<ProbedObservation, storage_runtime::host::HostError> {
        panic!("restart must inspect the checkpoint before probing");
    }
    /// A prior crash cannot be converted into an ordinary fresh admission attempt.
    fn check_admission(
        &mut self,
        _: &ExecutionRun,
        _: ActionId,
        _: &ProbedObservation,
        _: &storage_runtime::bindings::OutputBindings,
        _: &storage_runtime::lease::ExecutionLease,
    ) -> Result<storage_runtime::host::AdmissionDecision, storage_runtime::host::HostError> {
        panic!("restart must not request admission");
    }
}

/// Neither full-run nor single-action entry points resume effects after an actual coordinator death.
#[test]
fn coordinator_restart_blocks_before_probe_and_dispatch() -> Result<(), Box<dyn Error>> {
    use storage_runtime::{coordinator::CoordinatorError, host::HostError};
    let _process = isolate_processes();
    for phase in ["armed", "running", "completed", "verified_before_clear"] {
        let scratch = kill_at(phase)?;
        let execution = admission::Scratch(scratch.0.join("execution"));
        let bytes = fs::read(execution.0.join("active-attempt.json"))?;
        let (state, frozen) = fixture()?;
        let mut run = ExecutionRun::new(frozen.document())?;
        let fake = coordination::dispatcher(&execution, Rc::new(RefCell::new(state)));
        let calls = fake.calls.clone();
        let mut coordinator =
            ExecutionCoordinator::new(ExecutionRoot::open(&execution.0)?, NoProbe, fake, &run);
        assert!(matches!(
            coordinator.execute_run(&mut run),
            Err(CoordinatorError::Host(HostError::ActiveAttempt))
        ));
        assert_eq!(run.state(ActionId::new(0))?, ActionRunState::Waiting);
        assert!(matches!(
            coordinator.execute_action(&mut run, ActionId::new(0), None),
            Err(CoordinatorError::Host(HostError::ActiveAttempt))
        ));
        assert_eq!(calls.get(), 0);
        assert!(!run.is_verified());
        assert_eq!(fs::read(execution.0.join("active-attempt.json"))?, bytes);
        assert!(ExecutionRoot::open(&execution.0)?.try_lock_host().is_ok());
    }
    Ok(())
}

/// Observer unwinding cannot clear an armed marker or make the same coordinator retry an effect.
#[test]
fn observer_panic_retains_stop_latch_and_checkpoint() -> Result<(), Box<dyn Error>> {
    use storage_runtime::coordinator::CoordinatorError;
    let _process = isolate_processes();
    for phase in [ActionRunState::Admitted, ActionRunState::Verified] {
        let scratch = admission::Scratch::new()?;
        let (state, frozen) = fixture()?;
        let state = Rc::new(RefCell::new(state));
        let mut run = ExecutionRun::new(frozen.document())?;
        let mut fake = coordination::dispatcher(&scratch, state.clone());
        fake.created_uuid = Some("created-lv".into());
        let calls = fake.calls.clone();
        let mut coordinator = ExecutionCoordinator::new(
            ExecutionRoot::open(&scratch.0)?,
            coordination::host(state, &run)?,
            fake,
            &run,
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            coordinator.execute_run_with_updates(&mut run, &mut |run| {
                if run.state(ActionId::new(0)).ok() == Some(phase)
                    && scratch.0.join("active-attempt.json").exists()
                {
                    panic!("controlled observer failure at {phase:?}");
                }
            })
        }));
        assert!(result.is_err());
        let active = scratch.0.join("active-attempt.json");
        let bytes = fs::read(&active)?;
        assert_eq!(ActiveAttempt::from_bytes(&bytes)?.run_id, run.run_id());
        assert_eq!(run.state(ActionId::new(0))?, phase);
        assert!(!run.is_verified());
        let expected_calls = usize::from(phase == ActionRunState::Verified);
        assert_eq!(calls.get(), expected_calls);
        assert!(matches!(
            coordinator.execute_run(&mut run),
            Err(CoordinatorError::Stopped)
        ));
        assert_eq!(calls.get(), expected_calls);
        assert_eq!(fs::read(active)?, bytes);
        assert!(ExecutionRoot::open(&scratch.0)?.try_lock_host().is_ok());
    }
    Ok(())
}

/// Valid JSON with the same run/action IDs must not erase changed evidence during verified clear.
#[test]
fn verified_clear_rejects_changed_checkpoint_contract() -> Result<(), Box<dyn Error>> {
    use storage_runtime::{checkpoint::CheckpointError, coordinator::CoordinatorError};
    let _process = isolate_processes();
    let scratch = admission::Scratch::new()?;
    let (state, frozen) = fixture()?;
    let state = Rc::new(RefCell::new(state));
    let mut run = ExecutionRun::new(frozen.document())?;
    let mut fake = coordination::dispatcher(&scratch, state.clone());
    fake.created_uuid = Some("created-lv".into());
    let calls = fake.calls.clone();
    let mut coordinator = ExecutionCoordinator::new(
        ExecutionRoot::open(&scratch.0)?,
        coordination::host(state, &run)?,
        fake,
        &run,
    );
    let active = scratch.0.join("active-attempt.json");
    let mut changed = None;
    let result = coordinator.execute_run_with_updates(&mut run, &mut |run| {
        if changed.is_none() && run.state(ActionId::new(0)).ok() == Some(ActionRunState::Verified) {
            let bytes = fs::read(&active).unwrap_or_else(|error| panic!("marker: {error}"));
            let mut attempt = ActiveAttempt::from_bytes(&bytes)
                .unwrap_or_else(|error| panic!("attempt: {error}"));
            attempt.plan_digest = "b".repeat(64);
            let bytes = attempt
                .to_bytes()
                .unwrap_or_else(|error| panic!("changed: {error}"));
            fs::write(&active, &bytes).unwrap_or_else(|error| panic!("replacement: {error}"));
            changed = Some(bytes);
        }
    });
    assert!(matches!(
        result,
        Err(CoordinatorError::Checkpoint(
            CheckpointError::AttemptMismatch
        ))
    ));
    assert_eq!(calls.get(), 1);
    assert_eq!(run.state(ActionId::new(0))?, ActionRunState::Verified);
    assert!(!run.is_verified());
    assert_eq!(fs::read(active)?, changed.ok_or("injected replacement")?);
    assert!(matches!(
        coordinator.execute_run(&mut run),
        Err(CoordinatorError::Stopped)
    ));
    assert_eq!(calls.get(), 1);
    Ok(())
}
