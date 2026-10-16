//! Read-only incident review; journal claims never restore an executable run or permit retries.

use serde::Serialize;
use std::collections::HashMap;
use uuid::Uuid;

use super::{JournalError, JournalLog, JournalRecord, MAX_JOURNAL_RECORDS};
use crate::{
    ActionId, ActionRunState, BackendOutcome, ObservationReference, RunEventKind,
    editing::{DraftRevision, OperationId},
};

/// Conservative classification of recorded invocation information, not a recovery instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStatus {
    /// No admitted/invoked attempt is present in the complete record prefix.
    /// This never authorizes retry or establishes that the real device is unchanged.
    NoInvocationRecorded,
    /// Admission, Running, or intent was recorded without a complete actual completion record.
    InvocationUnconfirmed,
    /// A classified backend result and historical actual snapshot were recorded.
    /// Even Succeeded does not prove postconditions, preservation, or final goal success.
    CompletionRecorded {
        /// Backend's recorded classification, including failure or uncertainty.
        outcome: BackendOutcome,
        /// Reference of the actual snapshot retained in the matching completion record.
        observation: ObservationReference,
    },
}

/// One frozen action mapped back to the original user operation for incident display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveryAction {
    /// Action from the reviewed document.
    pub action: ActionId,
    /// Owner from the frozen revision, never a later draft.
    pub operation: OperationId,
    /// Last reported lifecycle state, not trusted proof that verification actually occurred.
    pub reported_state: ActionRunState,
    /// Recorded attempt/completion information for manual review and a fresh future probe.
    pub status: RecoveryStatus,
}

/// Historical incident report; there is deliberately no conversion into ExecutionRun or admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveryReview {
    /// Attempt whose records were reviewed.
    pub run_id: Uuid,
    /// Frozen description owning the recorded action scope.
    pub plan_id: Uuid,
    /// Original editing session.
    pub session_id: Uuid,
    /// Original frozen revision.
    pub revision: DraftRevision,
    /// Actions in the frozen document's order, with conservative attempt information.
    pub actions: Vec<RecoveryAction>,
    /// Interrupted last line, excluded from complete records and never repaired automatically.
    pub incomplete_tail: bool,
}

impl JournalLog {
    /// Validates event ordering and projects a read-only review without trusting recorded success.
    /// A fresh probe, explicit recovery policy, leases, and approval remain external requirements.
    pub fn review(&self) -> Result<RecoveryReview, JournalError> {
        if self.records.len() >= MAX_JOURNAL_RECORDS {
            return Err(JournalError::Limit);
        }
        self.header.validate()?;
        self.validate_records()?;
        let mut actions: HashMap<_, _> = self
            .header
            .operations
            .iter()
            .flat_map(|source| {
                source.actions.iter().map(|action| {
                    (
                        *action,
                        RecoveryAction {
                            action: *action,
                            operation: source.operation,
                            reported_state: ActionRunState::Waiting,
                            status: RecoveryStatus::NoInvocationRecorded,
                        },
                    )
                })
            })
            .collect();
        let mut admissions = HashMap::new();
        for record in &self.records {
            match record {
                JournalRecord::Header(_) => {
                    return Err(JournalError::Structure("duplicate header"));
                }
                JournalRecord::DispatchIntent { action, .. } => {
                    let report = actions
                        .get_mut(action)
                        .ok_or(JournalError::Structure("unknown action"))?;
                    if report.reported_state != ActionRunState::Running {
                        return Err(JournalError::Structure(
                            "intent precedes admitted Running state",
                        ));
                    }
                    report.status = RecoveryStatus::InvocationUnconfirmed;
                }
                JournalRecord::Completion(completion) => {
                    actions
                        .get_mut(&completion.action)
                        .ok_or(JournalError::Structure("unknown completion action"))?
                        .status = RecoveryStatus::CompletionRecorded {
                        outcome: completion.outcome.clone(),
                        observation: completion.observation.reference,
                    };
                }
                JournalRecord::Event(event) => match &event.kind {
                    RunEventKind::ActionState { action, state } => {
                        let previous = actions
                            .get(action)
                            .ok_or(JournalError::Structure("unknown action"))?
                            .reported_state;
                        let valid = matches!(
                            (previous, *state),
                            (ActionRunState::Waiting, ActionRunState::Checking)
                                | (
                                    ActionRunState::Checking,
                                    ActionRunState::Running | ActionRunState::Failed
                                )
                                | (
                                    ActionRunState::Running,
                                    ActionRunState::Verifying
                                        | ActionRunState::Failed
                                        | ActionRunState::OutcomeUnknown
                                )
                                | (
                                    ActionRunState::Verifying,
                                    ActionRunState::Verified
                                        | ActionRunState::Failed
                                        | ActionRunState::OutcomeUnknown
                                )
                        );
                        if !valid {
                            return Err(JournalError::Structure(
                                "impossible reported action transition",
                            ));
                        }
                        if *state == ActionRunState::Checking
                            && (actions.values().any(|report| {
                                matches!(
                                    report.reported_state,
                                    ActionRunState::Checking
                                        | ActionRunState::Running
                                        | ActionRunState::Verifying
                                        | ActionRunState::Failed
                                        | ActionRunState::OutcomeUnknown
                                )
                            }) || self
                                .header
                                .document
                                .dependencies
                                .iter()
                                .filter(|edge| edge.after == *action)
                                .any(|edge| {
                                    actions.get(&edge.before).is_none_or(|report| {
                                        report.reported_state != ActionRunState::Verified
                                    })
                                }))
                        {
                            return Err(JournalError::Structure(
                                "checking bypasses active/stopped action or predecessor",
                            ));
                        }
                        if previous == ActionRunState::Checking
                            && admissions.get(action) != Some(&(*state == ActionRunState::Running))
                        {
                            return Err(JournalError::Structure(
                                "transition has no matching admission event",
                            ));
                        }
                        let report = actions
                            .get_mut(action)
                            .ok_or(JournalError::Structure("unknown action"))?;
                        report.reported_state = *state;
                        if *state == ActionRunState::Running {
                            report.status = RecoveryStatus::InvocationUnconfirmed;
                        }
                    }
                    RunEventKind::Admission { action, admitted } => {
                        let report = actions
                            .get_mut(action)
                            .ok_or(JournalError::Structure("unknown action"))?;
                        if report.reported_state != ActionRunState::Checking {
                            return Err(JournalError::Structure("admission outside Checking"));
                        }
                        admissions.insert(*action, *admitted);
                        if *admitted {
                            report.status = RecoveryStatus::InvocationUnconfirmed;
                        }
                    }
                    RunEventKind::Progress { action, .. } => {
                        if actions
                            .get(action)
                            .is_none_or(|report| report.reported_state != ActionRunState::Running)
                        {
                            return Err(JournalError::Structure("progress outside Running"));
                        }
                    }
                    RunEventKind::GoalVerification { .. } => {
                        if actions
                            .values()
                            .any(|report| report.reported_state != ActionRunState::Verified)
                        {
                            return Err(JournalError::Structure(
                                "goal report precedes verified actions",
                            ));
                        }
                    }
                },
            }
        }
        Ok(RecoveryReview {
            run_id: self.header.run_id,
            plan_id: self.header.document.plan_id,
            session_id: self.header.session_id,
            revision: self.header.revision,
            actions: self
                .header
                .document
                .actions
                .iter()
                .map(|action| {
                    actions
                        .remove(&action.id)
                        .ok_or(JournalError::Structure("missing source action"))
                })
                .collect::<Result<_, _>>()?,
            incomplete_tail: self.incomplete_tail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RunEvent, journal::JournalHeader};

    /// An admitted intent remains uncertain, and no invalid reported transition can invent success.
    #[test]
    fn review_never_restores_success_or_retry_permission() -> Result<(), Box<dyn std::error::Error>>
    {
        let (frozen, run) = super::super::tests::frozen_run()?;
        let header = JournalHeader::capture(&frozen, &run)?;
        let action = ActionId::new(0);
        let records = [
            RunEventKind::ActionState {
                action,
                state: ActionRunState::Checking,
            },
            RunEventKind::Admission {
                action,
                admitted: true,
            },
            RunEventKind::ActionState {
                action,
                state: ActionRunState::Running,
            },
        ]
        .into_iter()
        .enumerate()
        .map(|(index, kind)| {
            JournalRecord::Event(RunEvent {
                run_id: run.run_id(),
                sequence: index as u64 + 1,
                kind,
            })
        })
        .collect();
        let mut log = JournalLog {
            header,
            records,
            incomplete_tail: false,
        };
        assert_eq!(
            log.review()?.actions[0].status,
            RecoveryStatus::InvocationUnconfirmed
        );
        log.records.push(JournalRecord::DispatchIntent {
            run_id: run.run_id(),
            action,
        });
        assert_eq!(
            log.review()?.actions[0].operation,
            frozen.source_map()[0].operation
        );
        log.records.push(JournalRecord::Event(RunEvent {
            run_id: run.run_id(),
            sequence: 4,
            kind: RunEventKind::ActionState {
                action,
                state: ActionRunState::Verified,
            },
        }));
        assert!(log.review().is_err());
        Ok(())
    }
}
