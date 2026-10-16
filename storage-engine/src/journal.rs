//! Bounded historical execution journals, never live authorization or executable recovery state.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    ActionId, BackendCompletion, ExecutionRun, PlanDocument, RunEvent,
    editing::{DraftRevision, FrozenPlan, OperationId},
};

mod file;
pub use file::DurableJournal;

/// Initial JSON-lines journal envelope, separate from plan/action schemas.
pub const JOURNAL_SCHEMA_VERSION: u32 = 1;
/// Maximum journal size accepted by this initial non-rotating format.
pub const MAX_JOURNAL_BYTES: usize = 16 * 1024 * 1024;
/// Bounds event/progress history before allocating an unbounded record list.
pub const MAX_JOURNAL_RECORDS: usize = 16_384;

/// Frozen user-operation ownership for progress and incident review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalOperation {
    /// Stable user request from the frozen revision.
    pub operation: OperationId,
    /// Exactly the actions generated for this request in the frozen plan.
    pub actions: Vec<ActionId>,
}

/// Historical context captured before any invocation, without credential material or live permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalHeader {
    /// Version of the journal record format.
    pub schema_version: u32,
    /// Attempt identity, independent of plan/session identities.
    pub run_id: Uuid,
    /// Editing session owning the reviewed revision.
    pub session_id: Uuid,
    /// Frozen revision, unaffected by later editing or undo.
    pub revision: DraftRevision,
    /// Exact immutable reviewed document, including destructive scope and historical observations.
    pub document: PlanDocument,
    /// Complete action ownership from this frozen revision.
    pub operations: Vec<JournalOperation>,
}

impl JournalHeader {
    /// Binds a run to the exact frozen description and copies its original source map.
    pub fn capture(frozen: &FrozenPlan, run: &ExecutionRun) -> Result<Self, JournalError> {
        if frozen.document().canonical_json()? != run.document().canonical_json()? {
            return Err(JournalError::Structure("run and frozen document differ"));
        }
        let header = Self {
            schema_version: JOURNAL_SCHEMA_VERSION,
            run_id: run.run_id(),
            session_id: frozen.session_id(),
            revision: frozen.revision(),
            document: frozen.document().clone(),
            operations: frozen
                .source_map()
                .iter()
                .map(|source| JournalOperation {
                    operation: source.operation,
                    actions: source.actions.clone(),
                })
                .collect(),
        };
        header.validate()?;
        Ok(header)
    }

    /// Validates imported historical context without restoring an ExecutionRun or authorizing it.
    fn validate(&self) -> Result<(), JournalError> {
        if self.schema_version != JOURNAL_SCHEMA_VERSION {
            return Err(JournalError::SchemaVersion(self.schema_version));
        }
        if self.run_id.is_nil() || self.session_id.is_nil() {
            return Err(JournalError::Structure(
                "journal identities must not be nil",
            ));
        }
        self.document.analyse_historical()?;
        let expected: HashSet<_> = self
            .document
            .actions
            .iter()
            .map(|action| action.id)
            .collect();
        let mut operations = HashSet::new();
        let mut actions = HashSet::new();
        for source in &self.operations {
            if source.operation.as_uuid().is_nil()
                || source.actions.is_empty()
                || !operations.insert(source.operation)
            {
                return Err(JournalError::Structure(
                    "invalid or duplicate source operation",
                ));
            }
            for action in &source.actions {
                if !expected.contains(action) || !actions.insert(*action) {
                    return Err(JournalError::Structure(
                        "unknown or duplicate source action",
                    ));
                }
            }
        }
        if actions != expected {
            return Err(JournalError::Structure(
                "source map does not cover every frozen action",
            ));
        }
        Ok(())
    }
}

/// A newline-terminated record; only a synced append can acknowledge durable recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum JournalRecord {
    /// Exactly one immutable first record.
    Header(Box<JournalHeader>),
    /// Existing engine event with its original run-local sequence.
    Event(RunEvent),
    /// Durable intent written after admission and before any backend call.
    DispatchIntent {
        /// Exact attempt owning this invocation.
        run_id: Uuid,
        /// One frozen action; the header retains its complete reviewed parameters.
        action: ActionId,
    },
    /// Classified completion and actual post-backend observation, including failure/unknown cases.
    Completion(Box<BackendCompletion>),
}

/// Parsed historical records; imported claims never establish leases, approval, or verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalLog {
    /// Validated frozen context from the first complete record.
    pub header: JournalHeader,
    /// Complete subsequent records, preserving original ordering.
    pub records: Vec<JournalRecord>,
    /// A final record was interrupted before its newline; complete prefix records remain available.
    pub incomplete_tail: bool,
}

/// Journal failure; callers must not dispatch after a failed durable append.
#[derive(Debug, Error)]
pub enum JournalError {
    /// Filesystem creation, write, sync, or bounded-read failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Malformed or unknown JSON fields in a complete record.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Invalid or unsupported frozen historical description.
    #[error(transparent)]
    Document(#[from] crate::PlanDocumentError),
    /// Unsupported journal envelope.
    #[error("unsupported journal schema {0}")]
    SchemaVersion(u32),
    /// Bounded size/count exhausted before further writing or parsing.
    #[error("journal exceeds its byte or record limit")]
    Limit,
    /// A failed write/sync makes further appends unsafe; this handle cannot be resumed.
    #[error("journal is unusable after an I/O failure")]
    Poisoned,
    /// The run has no admissible state or completion for the requested record.
    #[error(transparent)]
    Run(#[from] crate::ExecutionRunError),
    /// Invalid identity, scope, sequence, or record association.
    #[error("invalid journal: {0}")]
    Structure(&'static str),
}

impl JournalLog {
    /// Parses only complete lines, rejecting corrupt complete records and retaining a torn tail flag.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, JournalError> {
        if bytes.len() > MAX_JOURNAL_BYTES {
            return Err(JournalError::Limit);
        }
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .ok_or(JournalError::Structure("missing complete journal header"))?;
        let mut records = Vec::new();
        for line in bytes[..complete].split_inclusive(|byte| *byte == b'\n') {
            if records.len() >= MAX_JOURNAL_RECORDS {
                return Err(JournalError::Limit);
            }
            records.push(serde_json::from_slice::<JournalRecord>(line)?);
        }
        let JournalRecord::Header(header) = records.remove(0) else {
            return Err(JournalError::Structure("first record must be the header"));
        };
        header.validate()?;
        let log = Self {
            header: *header,
            records,
            incomplete_tail: complete != bytes.len(),
        };
        log.validate_records()?;
        Ok(log)
    }

    /// Checks run/action associations and sequences without trusting recorded success as live evidence.
    fn validate_records(&self) -> Result<(), JournalError> {
        let known: HashSet<_> = self
            .header
            .document
            .actions
            .iter()
            .map(|action| action.id)
            .collect();
        let mut sequence = 0;
        let mut intents = HashSet::new();
        let mut completions = HashSet::new();
        let mut observation = None;
        for record in &self.records {
            match record {
                JournalRecord::Header(_) => {
                    return Err(JournalError::Structure("duplicate header"));
                }
                JournalRecord::Event(event) => {
                    if event.run_id != self.header.run_id || event.sequence != sequence + 1 {
                        return Err(JournalError::Structure("wrong event run or sequence"));
                    }
                    let action = match event.kind {
                        crate::RunEventKind::ActionState { action, .. }
                        | crate::RunEventKind::Progress { action, .. }
                        | crate::RunEventKind::Admission { action, .. } => Some(action),
                        crate::RunEventKind::GoalVerification { .. } => None,
                    };
                    if action.is_some_and(|action| !known.contains(&action)) {
                        return Err(JournalError::Structure(
                            "event references an unknown action",
                        ));
                    }
                    sequence = event.sequence;
                }
                JournalRecord::DispatchIntent { run_id, action } => {
                    if *run_id != self.header.run_id
                        || !known.contains(action)
                        || !intents.insert(*action)
                    {
                        return Err(JournalError::Structure(
                            "wrong or duplicate dispatch intent",
                        ));
                    }
                }
                JournalRecord::Completion(completion) => {
                    if completion.run_id != self.header.run_id
                        || !intents.contains(&completion.action)
                        || !completions.insert(completion.action)
                    {
                        return Err(JournalError::Structure(
                            "completion has no unique matching intent",
                        ));
                    }
                    let reference = completion.observation.reference;
                    if reference.probe_instance.is_nil()
                        || observation.is_some_and(|previous: crate::ObservationReference| {
                            previous.probe_instance != reference.probe_instance
                                || previous.epoch >= reference.epoch
                        })
                    {
                        return Err(JournalError::Structure(
                            "completion observation is stale or foreign",
                        ));
                    }
                    completion
                        .observation
                        .snapshot
                        .graph
                        .validate_structure()
                        .map_err(|_| JournalError::Structure("invalid completion graph"))?;
                    observation = Some(reference);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editing::PlanSession;
    use storage_core::model::Bytes;

    /// Builds a frozen fixture and independent run without live checks or backend invocation.
    pub(super) fn frozen_run() -> Result<(FrozenPlan, ExecutionRun), Box<dyn std::error::Error>> {
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

    /// Header/source identity and event sequence are checked at the import boundary.
    #[test]
    fn journal_roundtrip_rejects_bad_scope_and_sequence() -> Result<(), Box<dyn std::error::Error>>
    {
        let (frozen, mut run) = frozen_run()?;
        let header = JournalHeader::capture(&frozen, &run)?;
        run.begin_check(ActionId::new(0))?;
        let mut bytes = serde_json::to_vec(&JournalRecord::Header(Box::new(header.clone())))?;
        bytes.push(b'\n');
        let prefix = bytes.clone();
        let event = run.events_since(0).next().ok_or("missing event")?.clone();
        bytes.extend(serde_json::to_vec(&JournalRecord::Event(event.clone()))?);
        bytes.push(b'\n');
        let log = JournalLog::from_bytes(&bytes)?;
        assert_eq!(log.header, header);
        assert_eq!(log.records, vec![JournalRecord::Event(event.clone())]);
        bytes.extend_from_slice(b"{\"kind\":");
        assert!(JournalLog::from_bytes(&bytes)?.incomplete_tail);
        let mut bad = prefix;
        let mut event = event;
        event.sequence = 2;
        bad.extend(serde_json::to_vec(&JournalRecord::Event(event))?);
        bad.push(b'\n');
        assert!(JournalLog::from_bytes(&bad).is_err());
        let mut header = header;
        header.operations.clear();
        assert!(header.validate().is_err());
        Ok(())
    }
}
