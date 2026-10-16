//! Read-only inspection of one interrupted attempt; never recovery, retry, or write admission.

use crate::{
    checkpoint::{ActiveAttempt, CheckpointError, CheckpointStore},
    lease::{ExecutionRoot, LeaseError},
    probe::{ProbeObservationError, ProbedObservation},
};
use serde::Serialize;
use storage_core::model::{DependencyKind, ExternalId, NodeKind, Presence};
use storage_engine::LiveObservation;
use storage_probe::LinuxProbe;
use storage_provider::{LvmAction, ProviderAction};
use thiserror::Error;

/// Inspection failures preserve the incident marker and block new execution.
#[derive(Debug, Error)]
pub enum IncidentError {
    /// The same host-wide OS lock used for execution could not be acquired.
    #[error(transparent)]
    Lease(#[from] LeaseError),
    /// The active file is unsafe, malformed, or unsupported.
    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),
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
