//! Read-only inspection of one interrupted attempt; never recovery, retry, or write admission.

use crate::{
    checkpoint::{ActiveAttempt, CheckpointError, CheckpointStore},
    lease::{ExecutionRoot, LeaseError},
    probe::{ProbeObservationError, ProbedObservation},
};
use serde::Serialize;
use storage_core::model::{DependencyKind, ExternalId, NodeKind, Presence};
use storage_engine::{HistoricalBaseline, LiveCheckStatus, LiveObservation, PlanDocumentError};
use storage_probe::LinuxProbe;
use storage_provider::{LvmAction, ProviderAction};
use thiserror::Error;
use uuid::Uuid;

/// Inspection failures preserve the incident marker and block new execution.
#[derive(Debug, Error)]
pub enum IncidentError {
    /// The same host-wide OS lock used for execution could not be acquired.
    #[error(transparent)]
    Lease(#[from] LeaseError),
    /// The active file is unsafe, malformed, or unsupported.
    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),
    /// Explicit confirmation must match the private token issued for this review.
    #[error("incident confirmation token does not match the reviewed attempt")]
    ConfirmationMismatch,
    /// The active attempt or the observations changed after review.
    #[error("incident changed; inspect and review it again")]
    Changed,
    /// A review from another configured execution directory cannot clear this root.
    #[error("incident review belongs to another execution root")]
    WrongRoot,
    /// Principal authorization or backend quiescence has not been established.
    #[error("incident acknowledgment is not safe: {0:?}")]
    Safety(LiveCheckStatus),
    /// Canonical observed-state comparison could not be completed.
    #[error(transparent)]
    Document(#[from] PlanDocumentError),
    /// Fresh provider data could not be collected or interpreted.
    #[error(transparent)]
    Probe(#[from] ProbeObservationError),
}

/// Observed diagnostic compatibility, not proof of causality, completion, or backend quiescence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentAssessment {
    /// The requested LV properties and owning VG are uniquely present; its creator is unknown.
    EffectObserved,
    /// Evidence is missing, contradictory, ambiguous, or unable to express this action's effect.
    Indeterminate,
}

/// One active attempt inspected against an actual provider pass while the shared lock was held.
#[derive(Debug, Serialize)]
pub struct IncidentInspection {
    /// Historical intent that may or may not have reached the backend.
    pub attempt: ActiveAttempt,
    /// Actual graph and probe reference; no predicted effect is applied to it.
    pub fresh_state: LiveObservation,
    /// Diagnostic compatibility only; even EffectObserved does not permit checkpoint removal.
    pub assessment: IncidentAssessment,
}

/// Inspects under the ordinary host lock without reconstructing an ExecutionRun or invoking a backend.
/// Missing files need no probe; malformed files fail closed and remain untouched.
pub fn inspect_incident(
    root: &ExecutionRoot,
    probe: &LinuxProbe<'_>,
) -> Result<Option<IncidentInspection>, IncidentError> {
    let lock = root.try_lock_host()?;
    inspect_locked(&CheckpointStore::new(&lock), probe)
}

/// Shares the same read/probe ordering with explicit review and acknowledgment under an existing lock.
fn inspect_locked(
    store: &CheckpointStore<'_>,
    probe: &LinuxProbe<'_>,
) -> Result<Option<IncidentInspection>, IncidentError> {
    let Some(attempt) = store.inspect()? else {
        return Ok(None);
    };
    let fresh_state = ProbedObservation::collect(probe)?.into_observation();
    let assessment = assess(&attempt, &fresh_state);
    Ok(Some(IncidentInspection {
        attempt,
        fresh_state,
        assessment,
    }))
}

/// Recognizes only a unique UUID-qualified VG and an LV with the exact requested geometry/layout.
/// Absence in a partial snapshot and activation state without native evidence remain indeterminate.
fn assess(attempt: &ActiveAttempt, observed: &LiveObservation) -> IncidentAssessment {
    if observed
        .snapshot
        .diagnostics
        .iter()
        .any(|item| item.severity != storage_core::model::DiagnosticSeverity::Warning)
    {
        return IncidentAssessment::Indeterminate;
    }
    let ProviderAction::Lvm(LvmAction::CreateLv {
        vg,
        name,
        size,
        layout,
        ..
    }) = &attempt.action
    else {
        return IncidentAssessment::Indeterminate;
    };
    let Some(fingerprint) = attempt
        .targets
        .iter()
        .find(|target| target.planned_id == *vg)
    else {
        return IncidentAssessment::Indeterminate;
    };
    let mut uuids = fingerprint
        .external_ids
        .iter()
        .filter(|id| matches!(id, ExternalId::LvmVgUuid(_)));
    let Some(identity) = uuids.next() else {
        return IncidentAssessment::Indeterminate;
    };
    if uuids.next().is_some() {
        return IncidentAssessment::Indeterminate;
    }
    let mut matches = observed
        .snapshot
        .graph
        .nodes()
        .filter(|(_, node)| node.size.identities.contains(identity));
    let Some((vg_id, node)) = matches.next() else {
        return IncidentAssessment::Indeterminate;
    };
    if matches.next().is_some()
        || node.size.presence != Presence::Present
        || !matches!(node.kind.kind, NodeKind::LvmVg { .. })
        || node
            .size
            .identities
            .iter()
            .filter(|id| matches!(id, ExternalId::LvmVgUuid(_)))
            .count()
            != 1
    {
        return IncidentAssessment::Indeterminate;
    }
    let mut candidates = observed
        .snapshot
        .graph
        .dependencies()
        .filter(|edge| edge.from == *vg_id && edge.kind == DependencyKind::Provides)
        .filter_map(|edge| {
            observed
                .snapshot
                .graph
                .node(&edge.to)
                .map(|node| (edge.to, node))
        })
        .filter(|(_, node)| matches!(&node.kind.kind, NodeKind::LvmLv(lv) if lv.name == *name));
    let Some((lv_id, node)) = candidates.next() else {
        return IncidentAssessment::Indeterminate;
    };
    if candidates.next().is_some()
        || node.size.presence != Presence::Present
        || node.kind.size != Some(*size)
        || !matches!(&node.kind.kind, NodeKind::LvmLv(lv) if lv.kind == *layout)
        || observed
            .snapshot
            .graph
            .dependencies()
            .filter(|edge| edge.to == lv_id)
            .count()
            != 1
    {
        return IncidentAssessment::Indeterminate;
    }
    let identities: Vec<_> = node.size.identities.iter().filter(|id| matches!(id, ExternalId::LvmLvUuid(uuid) if !uuid.trim().is_empty() && uuid.trim() == uuid)).collect();
    if identities.len() != 1
        || node
            .size
            .identities
            .iter()
            .filter(|id| matches!(id, ExternalId::LvmLvUuid(_)))
            .count()
            != 1
        || observed
            .snapshot
            .graph
            .nodes()
            .filter(|(_, node)| node.size.identities.contains(identities[0]))
            .count()
            != 1
    {
        return IncidentAssessment::Indeterminate;
    }
    IncidentAssessment::EffectObserved
}

/// One immutable inspection and its explicit confirmation token, tied to the actual root inode.
/// It cannot be deserialized, forged by editing JSON, or reused after acknowledgment.
#[derive(Debug)]
pub struct IncidentReview {
    /// Actual state the caller was shown, never editable through this review handle.
    inspection: IncidentInspection,
    /// Fresh confirmation challenge, not an authorization credential.
    token: Uuid,
    /// Device/inode of the private directory checked while the inspection lock was held.
    directory: (u64, u64),
}

impl IncidentReview {
    /// Borrows the complete inspection that requires conscious user review.
    pub fn inspection(&self) -> &IncidentInspection {
        &self.inspection
    }
    /// Returns the challenge the user must explicitly confirm for this exact review.
    pub fn confirmation_token(&self) -> Uuid {
        self.token
    }
}

/// Trusted principal and backend-safety policy; there is no production allow-all implementation.
/// Positive evidence must establish authorization, complete relevant coverage, and that any native
/// operation has actually stopped. A fresh graph or an available flock alone cannot establish this.
pub trait IncidentAcknowledgmentPolicy {
    /// Evaluates both the reviewed and fresh state while the shared host lock is still held.
    fn check(
        &mut self,
        reviewed: &IncidentInspection,
        fresh: &IncidentInspection,
    ) -> LiveCheckStatus;
}

/// Opens a new review without granting permission to clear or retry any action.
pub fn review_incident(
    root: &ExecutionRoot,
    probe: &LinuxProbe<'_>,
) -> Result<Option<IncidentReview>, IncidentError> {
    let lock = root.try_lock_host()?;
    let directory = directory_identity(&lock)?;
    Ok(
        inspect_locked(&CheckpointStore::new(&lock), probe)?.map(|inspection| IncidentReview {
            inspection,
            token: Uuid::now_v7(),
            directory,
        }),
    )
}

/// Consumes conscious confirmation, re-probes, and clears only the unchanged reviewed incident.
/// It never restores a run, marks an action Verified, retries it, or invokes a storage backend.
pub fn acknowledge_incident(
    root: &ExecutionRoot,
    probe: &LinuxProbe<'_>,
    review: IncidentReview,
    confirmation: Uuid,
    policy: &mut impl IncidentAcknowledgmentPolicy,
) -> Result<IncidentInspection, IncidentError> {
    if confirmation != review.token {
        return Err(IncidentError::ConfirmationMismatch);
    }
    let lock = root.try_lock_host()?;
    if directory_identity(&lock)? != review.directory {
        return Err(IncidentError::WrongRoot);
    }
    let store = CheckpointStore::new(&lock);
    if store.inspect()?.as_ref() != Some(&review.inspection.attempt) {
        return Err(IncidentError::Changed);
    }
    let fresh = inspect_locked(&store, probe)?.ok_or(IncidentError::Changed)?;
    if !normalized_observation(&review.inspection.fresh_state)?
        .has_same_observations(&normalized_observation(&fresh.fresh_state)?)?
    {
        return Err(IncidentError::Changed);
    }
    let safe = policy.check(&review.inspection, &fresh);
    if safe != LiveCheckStatus::Satisfied {
        return Err(IncidentError::Safety(safe));
    }
    // Trusted callbacks cannot accidentally authorize a replacement with the same run/action IDs.
    if store.inspect()?.as_ref() != Some(&review.inspection.attempt) {
        return Err(IncidentError::Changed);
    }
    store.clear(&fresh.attempt)?;
    Ok(fresh)
}

/// Compares observed content independently of publication epochs, retaining all identity and scope data.
fn normalized_observation(observed: &LiveObservation) -> Result<HistoricalBaseline, IncidentError> {
    let mut snapshot = observed.snapshot.clone();
    let nodes: Vec<_> = snapshot
        .graph
        .nodes()
        .map(|(id, node)| {
            let mut node = node.clone();
            node.size.observed_in = None;
            (*id, node)
        })
        .collect();
    for (id, node) in nodes {
        snapshot.graph.replace_existing(id, node).map_err(|error| {
            ProbeObservationError::Observation(storage_engine::live::ObservationError::from(error))
        })?;
    }
    Ok(HistoricalBaseline {
        id: observed.reference.probe_instance,
        snapshot,
        capacity_evidence: Vec::new(),
    })
}

/// Pins reviews to the checked directory, even if identical marker contents are copied elsewhere.
fn directory_identity(lock: &crate::lease::HostExecutionLock) -> Result<(u64, u64), IncidentError> {
    let info = rustix::fs::fstat(lock.directory()).map_err(|error| LeaseError::Io(error.into()))?;
    Ok((info.st_dev, info.st_ino))
}
