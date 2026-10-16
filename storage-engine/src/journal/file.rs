//! Exclusive new-file creation and acknowledged durable appends; no backend dispatch occurs here.

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

use super::{
    JournalError, JournalHeader, JournalLog, JournalRecord, MAX_JOURNAL_BYTES, MAX_JOURNAL_RECORDS,
};
use crate::{ActionId, ActionRunState, ExecutionRun, editing::FrozenPlan};

/// Single-owner journal handle. Existing files cannot be opened for append or automatically resumed.
/// Successful intent recording must precede backend dispatch under separately retained live leases.
pub struct DurableJournal {
    /// Exclusive newly created file, never exposed to callers or cloned by this API.
    file: File,
    /// Reviewed run and original operation ownership.
    header: JournalHeader,
    /// Complete records acknowledged by successful sync.
    records: usize,
    /// Byte count acknowledged by successful sync.
    bytes: usize,
    /// Last engine event durably acknowledged by this handle.
    event_sequence: u64,
    /// Actions with acknowledged invocation intent; retries require a new reviewed recovery policy.
    intents: HashSet<ActionId>,
    /// Actions whose actual classified completion was durably recorded.
    completions: HashSet<ActionId>,
    /// Any write/sync error permanently prevents further appends through this handle.
    poisoned: bool,
}

impl DurableJournal {
    /// Creates a private new journal before admission begins, syncs its header and parent directory.
    /// A failure may leave a partial file for inspection; it is never truncated or silently replaced.
    pub fn create(
        path: impl AsRef<Path>,
        frozen: &FrozenPlan,
        run: &ExecutionRun,
    ) -> Result<Self, JournalError> {
        let header = JournalHeader::capture(frozen, run)?;
        if run.events_since(0).next().is_some()
            || run
                .document()
                .actions
                .iter()
                .any(|action| run.state(action.id).ok() != Some(ActionRunState::Waiting))
        {
            return Err(JournalError::Structure(
                "journal must be created before run admission",
            ));
        }
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let directory = File::open(parent)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(path)?;
        let mut journal = Self {
            file,
            header,
            records: 0,
            bytes: 0,
            event_sequence: 0,
            intents: HashSet::new(),
            completions: HashSet::new(),
            poisoned: false,
        };
        journal.write_batch(&[JournalRecord::Header(Box::new(journal.header.clone()))])?;
        directory.sync_all()?;
        Ok(journal)
    }

    /// Returns the frozen review context; this is never substituted with a later editing draft.
    pub fn header(&self) -> &JournalHeader {
        &self.header
    }

    /// Syncs new engine events and returns their original last sequence for subscribers.
    /// A recorded Running event alone is not permission to invoke a backend.
    pub fn flush_events(&mut self, run: &ExecutionRun) -> Result<u64, JournalError> {
        self.check_run(run)?;
        let events = self.new_events(run);
        self.write_batch(&events)?;
        self.event_sequence = run
            .events_since(self.event_sequence)
            .last()
            .map_or(self.event_sequence, |event| event.sequence);
        Ok(self.event_sequence)
    }

    /// Syncs an exact admitted action's intent before the host may call its backend.
    /// This records an attempt, not live approval, OS leases, or a guarantee that invocation occurred.
    pub fn record_dispatch_intent(
        &mut self,
        run: &ExecutionRun,
        action: ActionId,
    ) -> Result<(), JournalError> {
        self.check_run(run)?;
        if run.state(action)? != ActionRunState::Running || self.intents.contains(&action) {
            return Err(JournalError::Structure(
                "intent requires one newly admitted invocation",
            ));
        }
        let mut records = self.new_events(run);
        records.push(JournalRecord::DispatchIntent {
            run_id: run.run_id(),
            action,
        });
        self.write_batch(&records)?;
        self.event_sequence = run
            .events_since(self.event_sequence)
            .last()
            .map_or(self.event_sequence, |event| event.sequence);
        self.intents.insert(action);
        Ok(())
    }

    /// Syncs retained actual post-backend state even when the backend failed or its outcome is unknown.
    /// Backend success remains unverified; no predicted effects are applied to the saved observation.
    pub fn record_completion(
        &mut self,
        run: &ExecutionRun,
        action: ActionId,
    ) -> Result<(), JournalError> {
        self.require_dispatch_intent(run, action)?;
        if self.completions.contains(&action) {
            return Err(JournalError::Structure(
                "completion requires one recorded intent",
            ));
        }
        let completion = run
            .completion(action)?
            .cloned()
            .ok_or(JournalError::Structure("run has no actual completion"))?;
        let mut records = vec![JournalRecord::Completion(Box::new(completion))];
        records.extend(self.new_events(run));
        self.write_batch(&records)?;
        self.event_sequence = run
            .events_since(self.event_sequence)
            .last()
            .map_or(self.event_sequence, |event| event.sequence);
        self.completions.insert(action);
        Ok(())
    }

    /// Requires an acknowledged intent on this healthy bound journal before collecting a result.
    pub(crate) fn require_dispatch_intent(
        &self,
        run: &ExecutionRun,
        action: ActionId,
    ) -> Result<(), JournalError> {
        self.check_run(run)?;
        if !self.intents.contains(&action) {
            return Err(JournalError::Structure(
                "completion requires one recorded intent",
            ));
        }
        Ok(())
    }

    /// Requires a healthy journal with an acknowledged actual completion before result verification.
    pub(crate) fn require_completion(
        &self,
        run: &ExecutionRun,
        action: ActionId,
    ) -> Result<(), JournalError> {
        self.require_dispatch_intent(run, action)?;
        if !self.completions.contains(&action) {
            return Err(JournalError::Structure(
                "verification requires an acknowledged completion",
            ));
        }
        Ok(())
    }

    /// Rejects cross-run/frozen-document mixing and any use following uncertain journal I/O.
    fn check_run(&self, run: &ExecutionRun) -> Result<(), JournalError> {
        if self.poisoned {
            return Err(JournalError::Poisoned);
        }
        if self.header.run_id != run.run_id()
            || self.header.document.canonical_json()? != run.document().canonical_json()?
        {
            return Err(JournalError::Structure(
                "journal belongs to another run or frozen document",
            ));
        }
        Ok(())
    }

    /// Copies existing events with their original sequence; it never creates runtime transitions.
    fn new_events(&self, run: &ExecutionRun) -> Vec<JournalRecord> {
        run.events_since(self.event_sequence)
            .cloned()
            .map(JournalRecord::Event)
            .collect()
    }

    /// Validates byte/count bounds before writing; counters advance only after successful sync.
    fn write_batch(&mut self, records: &[JournalRecord]) -> Result<(), JournalError> {
        if self.poisoned {
            return Err(JournalError::Poisoned);
        }
        if records.len() > MAX_JOURNAL_RECORDS - self.records {
            return Err(JournalError::Limit);
        }
        let mut encoded = Vec::new();
        for record in records {
            let line = serde_json::to_vec(record)?;
            if line.len() >= MAX_JOURNAL_BYTES - self.bytes - encoded.len() {
                return Err(JournalError::Limit);
            }
            encoded.extend(line);
            encoded.push(b'\n');
        }
        if encoded.is_empty() {
            return Ok(());
        }
        if let Err(error) = self
            .file
            .write_all(&encoded)
            .and_then(|()| self.file.sync_all())
        {
            self.poisoned = true;
            return Err(JournalError::Io(error));
        }
        self.records += records.len();
        self.bytes += encoded.len();
        Ok(())
    }
}

impl JournalLog {
    /// Reads a bounded regular journal file without opening an append or execution path.
    pub fn read(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let path = path.as_ref();
        if !std::fs::metadata(path)?.is_file() {
            return Err(JournalError::Structure("journal must be a regular file"));
        }
        let file = File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(JournalError::Structure("journal must be a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_JOURNAL_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Self::from_bytes(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Owns a unique test directory so failure cleanup cannot touch a caller's existing journal.
    struct Directory(PathBuf);
    impl Directory {
        /// Creates a new owned scratch directory without overwriting any path.
        fn new() -> std::io::Result<Self> {
            let path = std::env::temp_dir()
                .join(format!("disksgraph-journal-test-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir(&path)?;
            Ok(Self(path))
        }
    }
    impl Drop for Directory {
        /// Removes only this test's newly allocated scratch directory.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An unacknowledged completion stops even a successful backend; the actual probe is retained.
    #[cfg(target_os = "linux")]
    #[test]
    fn leased_completion_write_failure_retains_actual_state_and_stops()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{
            BackendOutcome, LiveCheck, LiveCheckStatus, journal::RecoveryStatus,
            leases::ExecutionLease, live::ProbedObservation,
        };
        use std::os::unix::fs::PermissionsExt;
        let directory = Directory::new()?;
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700))?;
        let path = directory.0.join("run.jsonl");
        let (frozen, mut run) = super::super::tests::frozen_run()?;
        let mut journal = DurableJournal::create(&path, &frozen, &run)?;
        let action = ActionId::new(0);
        run.begin_check(action)?;
        let guard = ExecutionLease::try_acquire(&directory.0, &run, action)?;
        let probe = storage_probe::LinuxProbe::from_providers(Vec::new());
        let observed = ProbedObservation::collect(&probe)?.into_observation();
        // Synthetic admission tests I/O handling only; no native backend/checker is invoked.
        let claims = run.context().plan.actions()[0]
            .informational_contract
            .prerequisites
            .iter()
            .cloned()
            .map(|condition| LiveCheck {
                condition,
                status: LiveCheckStatus::Satisfied,
            })
            .collect();
        guard.record_admission(&mut run, observed, LiveCheckStatus::Satisfied, claims)?;
        guard.record_dispatch_intent(&mut journal, &run)?;
        let before = std::fs::read(&path)?;
        journal.file = File::open(&path)?;
        assert!(
            guard
                .record_probed_completion(&mut journal, &mut run, &probe, BackendOutcome::Succeeded)
                .is_err()
        );
        assert_eq!(run.state(action)?, ActionRunState::OutcomeUnknown);
        assert!(run.completion(action)?.is_some());
        assert_eq!(
            run.latest_observation()
                .ok_or("actual observation")?
                .reference
                .epoch
                .get(),
            2
        );
        assert!(journal.poisoned);
        assert_eq!(std::fs::read(&path)?, before);
        assert_eq!(
            JournalLog::read(&path)?.review()?.actions[0].status,
            RecoveryStatus::InvocationUnconfirmed
        );
        assert!(run.begin_check(action).is_err());
        drop(guard);
        Ok(())
    }

    /// Existing files remain untouched, events flush once, and I/O errors poison the writer.
    #[test]
    fn journal_create_flush_and_failed_write_are_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = Directory::new()?;
        let path = directory.0.join("run.jsonl");
        let (frozen, mut run) = super::super::tests::frozen_run()?;
        let mut journal = DurableJournal::create(&path, &frozen, &run)?;
        let original = std::fs::read(&path)?;
        assert!(DurableJournal::create(&path, &frozen, &run).is_err());
        assert_eq!(std::fs::read(&path)?, original);
        run.begin_check(ActionId::new(0))?;
        assert!(
            journal
                .record_dispatch_intent(&run, ActionId::new(0))
                .is_err()
        );
        assert_eq!(journal.flush_events(&run)?, 1);
        let bytes = std::fs::read(&path)?;
        assert_eq!(journal.flush_events(&run)?, 1);
        assert_eq!(std::fs::read(&path)?, bytes);
        assert_eq!(JournalLog::read(&path)?.records.len(), 1);
        journal.file = File::open(&path)?;
        // A distinct event forces a write through a deliberately read-only test handle.
        let mut encoded = journal.new_events(&run);
        encoded.push(JournalRecord::DispatchIntent {
            run_id: run.run_id(),
            action: ActionId::new(0),
        });
        assert!(matches!(
            journal.write_batch(&encoded),
            Err(JournalError::Io(_))
        ));
        assert!(matches!(
            journal.flush_events(&run),
            Err(JournalError::Poisoned)
        ));
        assert_eq!(std::fs::read(&path)?, bytes);
        Ok(())
    }
}
