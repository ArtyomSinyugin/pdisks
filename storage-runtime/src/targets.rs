//! Resolve the first LVM recipe by stable identity before selecting any native endpoint.

use crate::checkpoint::TargetFingerprint;
use std::collections::HashMap;
use storage_core::model::{CurrentState, ExternalId, MountSource, NodeId, NodeKind};
use storage_engine::{
    ActionId, ExecutionRun, ExecutionRunError, LiveObservation, ObservationReference,
    editing::{CorrelationError, correlate_nodes},
    live::ObservationError,
};
use storage_provider::{BackendId, LvmAction, ProviderAction};
use thiserror::Error;
use uuid::Uuid;

/// Resolution failures never fall back to a display name, path, or reused transient NodeId.
#[derive(Debug, Error)]
pub enum TargetError {
    /// The observation or graph is malformed.
    #[error(transparent)]
    Observation(#[from] ObservationError),
    /// Stable correlation is missing, ambiguous, or incompatible.
    #[error(transparent)]
    Correlation(#[from] CorrelationError),
    /// The requested action does not belong to this run.
    #[error(transparent)]
    Run(#[from] ExecutionRunError),
    /// Native endpoint resolution currently supports only CreateLv in an existing VG.
    #[error("target resolution does not support this action")]
    UnsupportedAction,
    /// A reviewed property, dependency, relation, or mount changed in the affected component.
    #[error("live storage scope differs from the reviewed plan")]
    ScopeChanged,
    /// The requested LV name is already occupied in the resolved VG.
    #[error("requested LV name is already in use")]
    NameConflict,
    /// Fingerprint serialization failed without yielding an endpoint.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// A stable VG UUID and its currently observed native name, never an identity inferred from a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// Node identity retained in the reviewed plan.
    pub planned_id: NodeId,
    /// Node identity from this exact fresh observation.
    pub observed_id: NodeId,
    /// Validated LVM VG UUID shared by both observations.
    pub vg_uuid: String,
    /// Native name observed alongside that UUID; adapters must recheck it before mutation.
    pub vg_name: String,
}

/// An operation bound to one run, action, probe publication, and concrete backend.
/// Fields are private so consumers cannot replace the operation after admission.
#[derive(Debug)]
pub struct ResolvedAction {
    /// Attempt whose admission is being checked.
    run_id: Uuid,
    /// Exact frozen action index.
    action_id: ActionId,
    /// Typed operation copied from the trusted reconstructed plan.
    operation: ProviderAction,
    /// Concrete selected backend, never an arbitrary library path.
    backend: BackendId,
    /// Exact provider publication used for endpoint resolution.
    observation: ObservationReference,
    /// The sole existing metadata target in the initial recipe.
    target: ResolvedTarget,
}

impl ResolvedAction {
    /// Returns the run bound at resolution.
    pub fn run_id(&self) -> Uuid {
        self.run_id
    }
    /// Returns the action bound at resolution.
    pub fn action_id(&self) -> ActionId {
        self.action_id
    }
    /// Returns immutable typed storage intent.
    pub fn operation(&self) -> &ProviderAction {
        &self.operation
    }
    /// Returns the selected concrete backend.
    pub fn backend(&self) -> &BackendId {
        &self.backend
    }
    /// Returns the publication from which endpoint and scope were checked.
    pub fn observation(&self) -> ObservationReference {
        self.observation
    }
    /// Returns the UUID-qualified VG endpoint.
    pub fn target(&self) -> &ResolvedTarget {
        &self.target
    }
    /// Produces the minimal non-secret pre-dispatch fingerprint for the checkpoint.
    pub fn fingerprints(&self) -> Vec<TargetFingerprint> {
        vec![TargetFingerprint {
            planned_id: self.target.planned_id,
            external_ids: vec![ExternalId::LvmVgUuid(self.target.vg_uuid.clone())],
        }]
    }
}

/// Resolves the selected CreateLv target and checks observable scope without authorizing dispatch.
/// Unknown coverage must still be rejected by the trusted host's separate scope check.
pub fn resolve_action(
    run: &ExecutionRun,
    action: ActionId,
    observed: &LiveObservation,
    backend: BackendId,
) -> Result<ResolvedAction, TargetError> {
    observed.validate()?;
    let context = run.context();
    let planned = context
        .plan
        .actions()
        .iter()
        .find(|item| item.id == action)
        .ok_or(ExecutionRunError::UnknownAction(action))?;
    let ProviderAction::Lvm(LvmAction::CreateLv { vg, name, .. }) = &planned.operation else {
        return Err(TargetError::UnsupportedAction);
    };
    let baseline = &context.historical_baseline.snapshot;
    let mapping = correlate_nodes(baseline, &observed.snapshot, &[*vg])?;
    let target = &mapping[0];
    let node = observed
        .snapshot
        .graph
        .node(&target.target)
        .ok_or(TargetError::ScopeChanged)?;
    let (ExternalId::LvmVgUuid(uuid), NodeKind::LvmVg { name: vg_name, .. }) =
        (&target.identity, &node.kind.kind)
    else {
        return Err(TargetError::UnsupportedAction);
    };
    if observed
        .snapshot
        .graph
        .dependencies()
        .filter(|edge| edge.from == target.target)
        .any(|edge| {
            observed.snapshot.graph.node(&edge.to).is_some_and(|node| {
                matches!(&node.kind.kind,
            NodeKind::LvmLv(lv) if lv.name == *name)
            })
        })
    {
        return Err(TargetError::NameConflict);
    }
    check_scope(baseline, &observed.snapshot, *vg)?;
    Ok(ResolvedAction {
        run_id: run.run_id(),
        action_id: action,
        operation: planned.operation.clone(),
        backend,
        observation: observed.reference,
        target: ResolvedTarget {
            planned_id: *vg,
            observed_id: target.target,
            vg_uuid: uuid.clone(),
            vg_name: vg_name.clone(),
        },
    })
}

/// Conservatively compares the whole connected component, including existing LV consumers.
/// Unsupported identities prevent admission rather than silently reducing the checked scope.
pub(crate) fn check_scope(
    baseline: &CurrentState,
    current: &CurrentState,
    source: NodeId,
) -> Result<(), TargetError> {
    // Unsupported virtual-device identities remain fail-closed until their native scope recipe exists.
    let mut selected = vec![source];
    loop {
        let before = selected.len();
        for edge in baseline.graph.dependencies() {
            if selected.contains(&edge.from) && !selected.contains(&edge.to) {
                selected.push(edge.to);
            }
            if selected.contains(&edge.to) && !selected.contains(&edge.from) {
                selected.push(edge.from);
            }
        }
        for edge in baseline.graph.relations() {
            if selected.contains(&edge.from) && !selected.contains(&edge.to) {
                selected.push(edge.to);
            }
            if selected.contains(&edge.to) && !selected.contains(&edge.from) {
                selected.push(edge.from);
            }
        }
        if before == selected.len() {
            break;
        }
    }
    let mapping = correlate_nodes(baseline, current, &selected)?;
    let ids: HashMap<_, _> = mapping
        .iter()
        .map(|item| (item.source, item.target))
        .collect();
    for (old, new) in &ids {
        if baseline.graph.node(old).map(|node| &node.kind)
            != current.graph.node(new).map(|node| &node.kind)
        {
            return Err(TargetError::ScopeChanged);
        }
    }
    for (old, new) in &ids {
        let original = baseline.graph.node(old).ok_or(TargetError::ScopeChanged)?;
        if matches!(
            original.kind.kind,
            NodeKind::Disk | NodeKind::NvmeNamespace(_) | NodeKind::Partition { .. }
        ) {
            let current = current.graph.node(new).ok_or(TargetError::ScopeChanged)?;
            let (Some(before), Some(after)) = (&original.size.block, &current.size.block) else {
                return Err(TargetError::ScopeChanged);
            };
            if !original.kind.size.is_some_and(|size| size.as_u64() != 0)
                || before.geometry.is_none()
                || after.geometry != before.geometry
                || after.read_only != Some(false)
            {
                return Err(TargetError::ScopeChanged);
            }
        }
    }
    let dependencies: Vec<_> = baseline
        .graph
        .dependencies()
        .filter(|edge| ids.contains_key(&edge.from) || ids.contains_key(&edge.to))
        .map(|edge| {
            let mut edge = edge.clone();
            edge.from = ids[&edge.from];
            edge.to = ids[&edge.to];
            edge
        })
        .collect();
    let actual: Vec<_> = current
        .graph
        .dependencies()
        .filter(|edge| ids.values().any(|id| *id == edge.from || *id == edge.to))
        .collect();
    if dependencies.len() != actual.len() || dependencies.iter().any(|edge| !actual.contains(&edge))
    {
        return Err(TargetError::ScopeChanged);
    }
    let relations: Vec<_> = baseline
        .graph
        .relations()
        .filter(|edge| ids.contains_key(&edge.from) || ids.contains_key(&edge.to))
        .map(|edge| {
            let mut edge = edge.clone();
            edge.from = ids[&edge.from];
            edge.to = ids[&edge.to];
            edge
        })
        .collect();
    let actual: Vec<_> = current
        .graph
        .relations()
        .filter(|edge| ids.values().any(|id| *id == edge.from || *id == edge.to))
        .collect();
    if relations.len() != actual.len() || relations.iter().any(|edge| !actual.contains(&edge)) {
        return Err(TargetError::ScopeChanged);
    }
    let mut mounts = baseline.mounts.entries.clone();
    for mount in &mut mounts {
        match &mut mount.source {
            MountSource::Filesystem(id)
            | MountSource::BtrfsSubvolume(id)
            | MountSource::ZfsDataset(id) => {
                if let Some(target) = ids.get(id) {
                    *id = *target;
                }
            }
            _ => {}
        }
    }
    let mut expected: Vec<_> = mounts
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<_, _>>()?;
    let mut actual: Vec<_> = current
        .mounts
        .entries
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<_, _>>()?;
    expected.sort();
    actual.sort();
    if expected != actual {
        return Err(TargetError::ScopeChanged);
    }
    Ok(())
}
