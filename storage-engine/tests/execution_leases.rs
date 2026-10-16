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
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
    fs::hard_link(&target, &lock)?;
    assert!(matches!(
        ExecutionLease::try_acquire(&root.0, &run, action),
        Err(LeaseError::InvalidLockFile)
    ));
    fs::remove_file(&lock)?;
    fs::create_dir(&lock)?;
    assert!(ExecutionLease::try_acquire(&root.0, &run, action).is_err());
    fs::remove_dir(&lock)?;
    fs::write(&lock, b"persistent lock inode")?;
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600))?;
    let lease = ExecutionLease::try_acquire(&root.0, &run, action)?;
    drop(lease);
    assert_eq!(fs::read(&lock)?, b"persistent lock inode");
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644))?;
    assert!(matches!(
        ExecutionLease::try_acquire(&root.0, &run, action),
        Err(LeaseError::InvalidLockFile)
    ));
    let alias = root.0.join("alias");
    symlink(&root.0, &alias)?;
    assert!(ExecutionLease::try_acquire(&alias, &run, action).is_err());
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

/// Child entry point selected only by the parent test below; uses a test-owned temporary root.
#[test]
#[ignore = "subprocess helper invoked by lease_excludes_other_processes_and_survives_process_exit"]
fn lease_subprocess_child() -> Result<(), Box<dyn Error>> {
    let root = std::env::var_os("DISKSGRAPH_LEASE_TEST_ROOT").ok_or("missing subprocess root")?;
    let mode = std::env::var("DISKSGRAPH_LEASE_TEST_MODE")?;
    let (_, run) = fixture()?;
    let guard = ExecutionLease::try_acquire(PathBuf::from(root), &run, ActionId::new(0));
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
    let root = Scratch::new()?;
    let (_, run) = fixture()?;
    let action = ActionId::new(0);
    let lease = ExecutionLease::try_acquire(&root.0, &run, action)?;
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
    let lease = ExecutionLease::try_acquire(&root.0, &run, action)?;
    drop(lease);
    Ok(())
}

/// Supplies mutable test observations through the real probe assembler, without native adapters.
struct ActualProvider<'a> {
    /// Synthetic actual snapshot selected by the test after its simulated backend returns.
    state: &'a std::cell::RefCell<storage_core::model::CurrentState>,
    /// Confirms that rejected lifecycle/journal inputs do not trigger another probe.
    calls: &'a std::cell::Cell<u64>,
}
impl storage_probe::StateProvider for ActualProvider<'_> {
    /// Explicit identity of this test adapter.
    fn id(&self) -> &str {
        "completion-fixture"
    }
    /// Sole responsibility for its synthetic contribution.
    fn responsibility(&self) -> &str {
        "completion.fixture"
    }
    /// Uses normal in-process probe orchestration.
    fn connection_kind(&self) -> storage_provider::ProviderConnectionKind {
        storage_provider::ProviderConnectionKind::BuiltIn
    }
    /// Publishes only the selected actual fixture, without applying any predicted effect.
    fn probe(&self) -> Result<storage_probe::ProviderState, storage_probe::ProviderProbeError> {
        self.calls.set(self.calls.get() + 1);
        let state = self.state.borrow();
        Ok(storage_probe::ProviderState {
            graph: state.graph.clone(),
            mounts: state.mounts.entries.clone(),
            environment: Some(state.environment.clone()),
            diagnostics: state.diagnostics.clone(),
            lvm_vgs: Vec::new(),
        })
    }
}

/// All classified outcomes publish actual probe state and sync it under the retained guard.
#[test]
fn leased_completion_probes_and_journals_success_failure_and_unknown() -> Result<(), Box<dyn Error>>
{
    use storage_engine::{
        ActionRunState, BackendOutcome, LiveCheckStatus,
        journal::{DurableJournal, JournalLog, RecoveryStatus},
        live::ProbedObservation,
    };
    for (outcome, expected_state) in [
        (BackendOutcome::Succeeded, ActionRunState::Verifying),
        (
            BackendOutcome::Failed {
                reason: "fixture failure".into(),
            },
            ActionRunState::Failed,
        ),
        (
            BackendOutcome::OutcomeUnknown {
                reason: "fixture interruption".into(),
            },
            ActionRunState::OutcomeUnknown,
        ),
    ] {
        let root = Scratch::new()?;
        let (frozen, _) = fixture()?;
        let mut run = ExecutionRun::new(frozen.document())?;
        let action = ActionId::new(0);
        let path = root.0.join("run.jsonl");
        let mut journal = DurableJournal::create(&path, &frozen, &run)?;
        run.begin_check(action)?;
        let guard = ExecutionLease::try_acquire(&root.0, &run, action)?;
        let state = std::cell::RefCell::new(run.document().baseline.snapshot.clone());
        let calls = std::cell::Cell::new(0);
        let probe = storage_probe::LinuxProbe::from_providers(vec![Box::new(ActualProvider {
            state: &state,
            calls: &calls,
        })]);
        let observed = ProbedObservation::collect(&probe)?.into_observation();
        // Synthetic complete admission isolates completion wiring from the unfinished native checker.
        let claims = checks(&run, LiveCheckStatus::Satisfied);
        guard.record_admission(&mut run, observed, LiveCheckStatus::Satisfied, claims)?;
        assert!(
            guard
                .record_probed_completion(&mut journal, &mut run, &probe, outcome.clone())
                .is_err()
        );
        assert_eq!(calls.get(), 1);
        guard.record_dispatch_intent(&mut journal, &run)?;
        state.borrow_mut().graph = storage_core::model::NodeGraph::new();
        guard.record_probed_completion(&mut journal, &mut run, &probe, outcome.clone())?;
        assert_eq!(calls.get(), 2);
        assert_eq!(run.state(action)?, expected_state);
        let completion = run.completion(action)?.ok_or("actual completion")?;
        assert_eq!(completion.outcome, outcome);
        assert_eq!(completion.observation.reference.epoch.get(), 2);
        assert_eq!(completion.observation.snapshot, *state.borrow());
        assert_eq!(run.latest_observation(), Some(&completion.observation));
        assert!(
            matches!(&JournalLog::read(&path)?.review()?.actions[0].status,
            RecoveryStatus::CompletionRecorded { outcome: recorded, .. } if *recorded == outcome)
        );
        assert!(
            guard
                .record_probed_completion(&mut journal, &mut run, &probe, outcome)
                .is_err()
        );
        assert_eq!(calls.get(), 2);
        assert!(!run.is_verified());
        drop(guard);
    }
    Ok(())
}

/// Missing/foreign post-backend evidence stops the action and cannot replace the last actual state.
#[test]
fn completion_probe_failure_stops_without_retry_or_projected_success() -> Result<(), Box<dyn Error>>
{
    use storage_engine::{
        ActionRunState, BackendOutcome, LiveCheckStatus,
        journal::{DurableJournal, JournalLog, RecoveryStatus},
        live::ProbedObservation,
    };
    for foreign_probe in [false, true] {
        let root = Scratch::new()?;
        let (frozen, _) = fixture()?;
        let mut run = ExecutionRun::new(frozen.document())?;
        let action = ActionId::new(0);
        let path = root.0.join("run.jsonl");
        let mut journal = DurableJournal::create(&path, &frozen, &run)?;
        run.begin_check(action)?;
        let guard = ExecutionLease::try_acquire(&root.0, &run, action)?;
        let state = std::cell::RefCell::new(run.document().baseline.snapshot.clone());
        let calls = std::cell::Cell::new(0);
        let probe = storage_probe::LinuxProbe::from_providers(vec![Box::new(ActualProvider {
            state: &state,
            calls: &calls,
        })]);
        let other_probe =
            storage_probe::LinuxProbe::from_providers(vec![Box::new(ActualProvider {
                state: &state,
                calls: &calls,
            })]);
        let observed = ProbedObservation::collect(&probe)?.into_observation();
        let claims = checks(&run, LiveCheckStatus::Satisfied);
        guard.record_admission(&mut run, observed, LiveCheckStatus::Satisfied, claims)?;
        guard.record_dispatch_intent(&mut journal, &run)?;
        let retained = run.latest_observation().cloned();
        if !foreign_probe {
            state
                .borrow_mut()
                .graph
                .insert_dependency(storage_core::model::Dependency {
                    from: storage_core::model::NodeId::new(),
                    to: storage_core::model::NodeId::new(),
                    kind: storage_core::model::DependencyKind::Backs,
                });
        }
        let selected = if foreign_probe { &other_probe } else { &probe };
        assert!(
            guard
                .record_probed_completion(
                    &mut journal,
                    &mut run,
                    selected,
                    BackendOutcome::Succeeded
                )
                .is_err()
        );
        assert_eq!(run.state(action)?, ActionRunState::OutcomeUnknown);
        assert_eq!(run.latest_observation(), retained.as_ref());
        assert!(run.completion(action)?.is_none());
        assert!(run.unknown_reason(action)?.is_some());
        assert_eq!(
            JournalLog::read(&path)?.review()?.actions[0].status,
            RecoveryStatus::InvocationUnconfirmed
        );
        assert_eq!(calls.get(), 2);
        assert!(
            guard
                .record_probed_completion(&mut journal, &mut run, &probe, BackendOutcome::Succeeded)
                .is_err()
        );
        assert_eq!(calls.get(), 2);
        drop(guard);
    }
    Ok(())
}

/// Verification uses the exact durable completion, even when a later observation differs.
#[test]
fn leased_observed_verification_distinguishes_match_mismatch_and_missing_evidence()
-> Result<(), Box<dyn Error>> {
    use storage_core::model::NodeKind;
    use storage_engine::{
        ActionRunState, BackendCompletion, BackendOutcome, HistoricalBaseline, LiveCheckStatus,
        editing::{PlanEdit, PlanRequest},
        journal::{DurableJournal, JournalLog},
        live::ProbedObservation,
    };
    for case in 0..3 {
        let root = Scratch::new()?;
        let baseline: HistoricalBaseline =
            serde_json::from_str(include_str!("../examples/fixtures/resize_baseline.json"))?;
        let filesystem = *baseline
            .snapshot
            .graph
            .nodes()
            .find(|(_, node)| matches!(node.kind.kind, NodeKind::Filesystem { .. }))
            .ok_or("filesystem")?
            .0;
        let state = std::cell::RefCell::new(baseline.snapshot.clone());
        let mut session = PlanSession::new(baseline)?;
        session.edit(
            session.revision(),
            PlanEdit::Add(PlanRequest::SetFilesystemLabel {
                filesystem,
                label: Some("renamed".into()),
            }),
        )?;
        let frozen = session.freeze(session.revision())?;
        let mut run = ExecutionRun::new(frozen.document())?;
        let mut other = ExecutionRun::new(frozen.document())?;
        let path = root.0.join("run.jsonl");
        let mut journal = DurableJournal::create(&path, &frozen, &run)?;
        let other_journal = DurableJournal::create(root.0.join("other.jsonl"), &frozen, &other)?;
        let action = ActionId::new(0);
        run.begin_check(action)?;
        other.begin_check(action)?;
        let guard = ExecutionLease::try_acquire(&root.0, &run, action)?;
        let calls = std::cell::Cell::new(0);
        let probe = storage_probe::LinuxProbe::from_providers(vec![Box::new(ActualProvider {
            state: &state,
            calls: &calls,
        })]);
        let observed = ProbedObservation::collect(&probe)?.into_observation();
        // Synthetic complete admission and classified result exercise verification wiring only.
        let claims = checks(&run, LiveCheckStatus::Satisfied);
        guard.record_admission(&mut run, observed, LiveCheckStatus::Satisfied, claims)?;
        guard.record_dispatch_intent(&mut journal, &run)?;
        let original_node = state
            .borrow()
            .graph
            .node(&filesystem)
            .ok_or("filesystem")?
            .clone();
        let mut node = original_node.clone();
        if case != 1 {
            let NodeKind::Filesystem { label, .. } = &mut node.kind.kind else {
                return Err("filesystem kind".into());
            };
            *label = Some("renamed".into());
        }
        if case == 2 {
            node.size.identities.clear();
        }
        state
            .borrow_mut()
            .graph
            .replace_existing(filesystem, node)?;
        if case == 0 {
            let completion = BackendCompletion {
                run_id: run.run_id(),
                action,
                outcome: BackendOutcome::Succeeded,
                observation: ProbedObservation::collect(&probe)?.into_observation(),
            };
            run.record_completion(completion)?;
            assert!(
                guard
                    .record_observed_verification(&journal, &mut run)
                    .is_err()
            );
            assert_eq!(run.state(action)?, ActionRunState::Verifying);
            assert!(run.verification(action)?.is_none());
            journal.record_completion(&run, action)?;
        } else {
            guard.record_probed_completion(
                &mut journal,
                &mut run,
                &probe,
                BackendOutcome::Succeeded,
            )?;
        }
        let reference = run
            .completion(action)?
            .ok_or("retained completion")?
            .observation
            .reference;
        assert!(
            guard
                .record_observed_verification(&other_journal, &mut run)
                .is_err()
        );
        assert!(run.verification(action)?.is_none());
        // A later snapshot cannot replace the publication retained for this result's postconditions.
        state
            .borrow_mut()
            .graph
            .replace_existing(filesystem, original_node)?;
        let later = ProbedObservation::collect(&probe)?;
        assert_ne!(later.observation().reference, reference);
        assert_eq!(
            guard.record_observed_verification(&journal, &mut run)?,
            case == 0
        );
        let expected = match case {
            0 => ActionRunState::Verified,
            1 => ActionRunState::Failed,
            _ => ActionRunState::OutcomeUnknown,
        };
        assert_eq!(run.state(action)?, expected);
        assert_eq!(
            run.verification(action)?.ok_or("verification")?.observation,
            reference
        );
        assert_eq!(calls.get(), 3);
        journal.flush_events(&run)?;
        assert_eq!(
            JournalLog::read(&path)?.review()?.actions[0].reported_state,
            expected
        );
        assert!(matches!(
            ExecutionLease::try_acquire(&root.0, &other, action),
            Err(LeaseError::Busy)
        ));
        assert!(!run.is_verified());
        assert!(
            guard
                .record_observed_verification(&journal, &mut run)
                .is_err()
        );
        drop(guard);
    }
    Ok(())
}
