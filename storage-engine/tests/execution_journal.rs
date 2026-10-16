//! Journal I/O and crash-prefix tests use a synthetic host only; no native backend is invoked.

use std::{error::Error, path::PathBuf};
use storage_core::model::{Bytes, ProbeEpoch};
use storage_engine::{
    ActionId, ActionRunState, AdmissionReport, BackendCompletion, BackendOutcome, ExecutionRun,
    LiveCheck, LiveCheckStatus, LiveObservation, ObservationReference,
    editing::{FrozenPlan, PlanSession},
    journal::{
        DurableJournal, JournalError, JournalHeader, JournalLog, JournalRecord, MAX_JOURNAL_BYTES,
        RecoveryStatus,
    },
};
use uuid::Uuid;

/// Owns one unique scratch path; existing caller-owned paths are never removed by these tests.
struct Scratch(PathBuf);
impl Scratch {
    /// Reserves a unique name; DurableJournal itself must create the file exclusively.
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("pdisks-journal-{}.jsonl", Uuid::now_v7())))
    }
}
impl Drop for Scratch {
    /// Removes only the test's unique newly created file.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Creates a reviewed LV plan and waiting run from a safe synthetic fixture.
fn fixture() -> Result<(FrozenPlan, ExecutionRun), Box<dyn Error>> {
    let baseline =
        serde_json::from_str(include_str!("../examples/fixtures/editing_baseline.json"))?;
    let mut session = PlanSession::new(baseline)?;
    let storage_provider::AllocationDomain::LvmVolumeGroup(vg) =
        session.baseline().capacity_evidence[0].domain;
    session.add_lv(session.revision(), vg, "data", Bytes::new(16 * 1024 * 1024))?;
    let frozen = session.freeze(session.revision())?;
    let run = ExecutionRun::new(frozen.document())?;
    Ok((frozen, run))
}

/// Synthetic host attestation for lifecycle tests, never an implementation of native live checks.
fn admit(run: &mut ExecutionRun, action: ActionId, instance: Uuid) -> Result<(), Box<dyn Error>> {
    run.begin_check(action)?;
    let contract = &run.context().plan.actions()[action.get() as usize].informational_contract;
    let report = AdmissionReport {
        run_id: run.run_id(),
        action,
        observation: LiveObservation {
            reference: ObservationReference {
                probe_instance: instance,
                epoch: ProbeEpoch::new(1),
            },
            snapshot: run.document().baseline.snapshot.clone(),
        },
        applicability: LiveCheckStatus::Satisfied,
        held_resources: contract.affected_resources.clone(),
        checks: contract
            .prerequisites
            .iter()
            .cloned()
            .map(|condition| LiveCheck {
                condition,
                status: LiveCheckStatus::Satisfied,
            })
            .collect(),
    };
    assert!(run.record_admission(report)?);
    Ok(())
}

/// Failed/unknown/successful completions retain actual fixture state, never the predicted created LV.
#[test]
fn journal_records_intent_before_completion_and_preserves_actual_snapshots()
-> Result<(), Box<dyn Error>> {
    for outcome in [
        BackendOutcome::Succeeded,
        BackendOutcome::Failed {
            reason: "fixture failure".to_owned(),
        },
        BackendOutcome::OutcomeUnknown {
            reason: "fixture interruption".to_owned(),
        },
    ] {
        let (frozen, mut run) = fixture()?;
        let path = Scratch::new();
        let mut journal = DurableJournal::create(&path.0, &frozen, &run)?;
        let action = ActionId::new(0);
        let instance = Uuid::now_v7();
        admit(&mut run, action, instance)?;
        journal.record_dispatch_intent(&run, action)?;
        let before_completion = std::fs::read(&path.0)?;
        let review = JournalLog::from_bytes(&before_completion)?.review()?;
        assert_eq!(
            review.actions[0].status,
            RecoveryStatus::InvocationUnconfirmed
        );
        assert_eq!(
            review.actions[0].operation,
            frozen.source_map()[0].operation
        );
        assert!(journal.record_dispatch_intent(&run, action).is_err());
        assert!(journal.record_completion(&run, action).is_err());
        assert_eq!(std::fs::read(&path.0)?, before_completion);
        let completion = BackendCompletion {
            run_id: run.run_id(),
            action,
            outcome: outcome.clone(),
            observation: LiveObservation {
                reference: ObservationReference {
                    probe_instance: instance,
                    epoch: ProbeEpoch::new(2),
                },
                snapshot: frozen.document().baseline.snapshot.clone(),
            },
        };
        run.record_completion(completion.clone())?;
        // Historical imports obey the same envelope checks as live runtime reports.
        for case in 0..2 {
            let mut invalid = completion.clone();
            if case == 0 {
                let (id, mut node) = invalid
                    .observation
                    .snapshot
                    .graph
                    .nodes()
                    .next()
                    .map(|(id, node)| (*id, node.clone()))
                    .ok_or("fixture node")?;
                node.size.observed_in = Some(ProbeEpoch::new(99));
                invalid
                    .observation
                    .snapshot
                    .graph
                    .replace_existing(id, node)?;
            } else {
                invalid.observation.snapshot.mounts.entries.push(
                    storage_core::model::ObservedMount {
                        source: storage_core::model::MountSource::Filesystem(
                            storage_core::model::NodeId::new(),
                        ),
                        target: "/dangling".into(),
                        options: Vec::new(),
                        context: storage_core::model::MountContext::Host,
                    },
                );
            }
            let mut bytes = before_completion.clone();
            bytes.extend(serde_json::to_vec(&JournalRecord::Completion(Box::new(
                invalid,
            )))?);
            bytes.push(b'\n');
            assert!(JournalLog::from_bytes(&bytes).is_err());
        }
        journal.record_completion(&run, action)?;
        let log = JournalLog::read(&path.0)?;
        let review = log.review()?;
        assert!(
            matches!(&review.actions[0].status, RecoveryStatus::CompletionRecorded { outcome: actual, .. } if actual == &outcome)
        );
        let recorded = log
            .records
            .iter()
            .find_map(|record| match record {
                JournalRecord::Completion(value) => Some(value),
                _ => None,
            })
            .ok_or("missing actual completion")?;
        assert_eq!(recorded.as_ref(), &completion);
        assert_eq!(
            recorded.observation.snapshot,
            frozen.document().baseline.snapshot
        );
        assert_ne!(review.actions[0].reported_state, ActionRunState::Verified);
        assert!(!run.is_verified());
        let complete_bytes = std::fs::read(&path.0)?;
        assert!(journal.record_completion(&run, action).is_err());
        assert_eq!(std::fs::read(&path.0)?, complete_bytes);

        let mut line = serde_json::to_vec(&JournalRecord::Completion(Box::new(completion)))?;
        line.push(b'\n');
        for cut in [1, line.len() / 2, line.len() - 1] {
            let mut interrupted = before_completion.clone();
            interrupted.extend_from_slice(&line[..cut]);
            let recovered = JournalLog::from_bytes(&interrupted)?;
            assert!(recovered.incomplete_tail);
            assert_eq!(
                recovered.review()?.actions[0].status,
                RecoveryStatus::InvocationUnconfirmed
            );
        }
    }
    Ok(())
}

/// Unknown live admission cannot create an invocation intent, and a foreign run cannot append.
#[test]
fn journal_does_not_turn_waiting_or_unknown_evidence_into_dispatch() -> Result<(), Box<dyn Error>> {
    let (frozen, mut run) = fixture()?;
    let path = Scratch::new();
    let mut journal = DurableJournal::create(&path.0, &frozen, &run)?;
    let action = ActionId::new(0);
    let header_bytes = std::fs::read(&path.0)?;
    assert!(journal.record_dispatch_intent(&run, action).is_err());
    assert_eq!(std::fs::read(&path.0)?, header_bytes);
    run.begin_check(action)?;
    let contract = &run.context().plan.actions()[0].informational_contract;
    // A synthetic host can report every condition while explicitly withholding applicability.
    let report = AdmissionReport {
        run_id: run.run_id(),
        action,
        observation: LiveObservation {
            reference: ObservationReference {
                probe_instance: Uuid::now_v7(),
                epoch: ProbeEpoch::new(1),
            },
            snapshot: run.document().baseline.snapshot.clone(),
        },
        applicability: LiveCheckStatus::Unknown {
            required_evidence: "fixture has no native runtime".to_owned(),
        },
        held_resources: contract.affected_resources.clone(),
        checks: contract
            .prerequisites
            .iter()
            .cloned()
            .map(|condition| LiveCheck {
                condition,
                status: LiveCheckStatus::Satisfied,
            })
            .collect(),
    };
    assert!(!run.record_admission(report)?);
    journal.flush_events(&run)?;
    assert!(journal.record_dispatch_intent(&run, action).is_err());
    assert_eq!(
        JournalLog::read(&path.0)?.review()?.actions[0].status,
        RecoveryStatus::NoInvocationRecorded
    );
    let mut foreign = ExecutionRun::new(frozen.document())?;
    foreign.begin_check(action)?;
    assert!(journal.flush_events(&foreign).is_err());
    assert!(JournalHeader::capture(&fixture()?.0, &run).is_err());
    let other = Scratch::new();
    assert!(DurableJournal::create(&other.0, &frozen, &run).is_err());
    assert!(!other.0.exists());
    Ok(())
}

/// Unsupported/corrupt input fails closed; torn headers cannot be mistaken for a complete context.
#[test]
fn journal_import_rejects_corruption_versions_scope_and_bounds() -> Result<(), Box<dyn Error>> {
    let (frozen, run) = fixture()?;
    let header = JournalHeader::capture(&frozen, &run)?;
    let mut bytes = serde_json::to_vec(&JournalRecord::Header(Box::new(header.clone())))?;
    assert!(JournalLog::from_bytes(&bytes).is_err());
    bytes.push(b'\n');
    assert!(JournalLog::from_bytes(&bytes).is_ok());
    for case in 0..5 {
        let mut changed = header.clone();
        match case {
            0 => changed.schema_version += 1,
            1 => changed.run_id = Uuid::nil(),
            2 => {
                let action = changed.operations[0].actions[0];
                changed.operations[0].actions.push(action);
            }
            3 => changed.operations[0].actions[0] = ActionId::new(999),
            _ => changed.document.plan_id = Uuid::nil(),
        }
        let mut encoded = serde_json::to_vec(&JournalRecord::Header(Box::new(changed)))?;
        encoded.push(b'\n');
        assert!(JournalLog::from_bytes(&encoded).is_err());
    }
    let mut duplicate_header = bytes.clone();
    duplicate_header.extend_from_slice(&bytes);
    assert!(JournalLog::from_bytes(&duplicate_header).is_err());
    let mut corrupt = bytes.clone();
    corrupt.extend_from_slice(b"{bad}\n");
    assert!(JournalLog::from_bytes(&corrupt).is_err());
    let mut future = serde_json::to_value(JournalRecord::Header(Box::new(header)))?;
    future["value"]["unexpected"] = serde_json::json!(true);
    let mut unknown_field = serde_json::to_vec(&future)?;
    unknown_field.push(b'\n');
    assert!(JournalLog::from_bytes(&unknown_field).is_err());
    assert!(matches!(
        JournalLog::from_bytes(&vec![b' '; MAX_JOURNAL_BYTES + 1]),
        Err(JournalError::Limit)
    ));
    Ok(())
}
