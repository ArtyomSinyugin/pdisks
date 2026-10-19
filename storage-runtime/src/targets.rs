//! Resolve the first LVM recipe by stable identity before selecting any native endpoint.

use crate::{bindings::OutputBindings, checkpoint::TargetFingerprint};
use std::collections::HashMap;
use storage_core::model::{
    BlockFacts, Bytes, CurrentState, ExternalId, FilesystemKind, LvmLvKind, MountSource, NodeId,
    NodeKind,
};
use storage_engine::{
    ActionId, ExecutionRun, ExecutionRunError, LiveObservation, ObservationReference,
    editing::{CorrelationError, correlate_nodes},
    live::ObservationError,
};
use storage_provider::{
    BackendId, FilesystemAction, LinearLvRequest, LvActivationRequest, LvmAction, LvmWriteError,
    MountAction, ProviderAction,
};
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
    /// Only linear creation, activation, ext4 creation and Host mount of verified outputs are resolved.
    #[error("target resolution does not support this action")]
    UnsupportedAction,
    /// Ext4 creation requires an active writable verified LV and representable label.
    #[error("ext4 creation requires an active unused verified LV with a known block endpoint")]
    FilesystemEndpoint,
    /// A planned LV has no verified identity in this attempt, or that identity changed.
    #[error("target is not an unchanged verified output of this run")]
    UnverifiedOutput,
    /// The resolved native endpoint or parameters fail the narrow recipe's validation.
    #[error(transparent)]
    NativeRequest(#[from] LvmWriteError),
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

/// UUID-qualified LV endpoint from a verified output and this exact provider publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLvTarget {
    /// Output node reserved by the frozen plan.
    pub planned_id: NodeId,
    /// Freshly correlated transient node identity.
    pub observed_id: NodeId,
    /// Immutable LV UUID, never inferred from its name or block path.
    pub lv_uuid: String,
    /// Native LV name observed alongside the UUID and owning VG.
    pub lv_name: String,
    /// Exact reviewed capacity, rechecked natively before activation.
    pub size: Bytes,
    /// Block facts from this exact probe; these are observations, not a held native descriptor.
    pub block: Option<BlockFacts>,
}

/// Ext4 identity correlated from this run's verified output in the exact admission publication.
#[derive(Debug)]
pub struct ResolvedFilesystemTarget {
    /// Planned output retained by the immutable plan.
    pub planned_id: NodeId,
    /// Fresh provider identity.
    pub observed_id: NodeId,
    /// Independently verified ext4 UUID.
    pub uuid: String,
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
    /// Present for activation, ext4 creation or mount on a verified created LV.
    lv_target: Option<ResolvedLvTarget>,
    /// Present only for a Host mount of this run's verified ext4 output.
    filesystem_target: Option<ResolvedFilesystemTarget>,
}

impl ResolvedAction {
    /// Returns the UUID-qualified filesystem whose backing LV is retained by the mount adapter.
    pub fn filesystem_target(&self) -> Option<&ResolvedFilesystemTarget> {
        self.filesystem_target.as_ref()
    }
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
    /// Returns the verified LV endpoint for activation or ext4 creation, if this recipe uses one.
    pub fn lv_target(&self) -> Option<&ResolvedLvTarget> {
        self.lv_target.as_ref()
    }
    /// Produces the minimal non-secret pre-dispatch fingerprint for the checkpoint.
    pub fn fingerprints(&self) -> Vec<TargetFingerprint> {
        let mut targets = vec![TargetFingerprint {
            planned_id: self.target.planned_id,
            external_ids: vec![ExternalId::LvmVgUuid(self.target.vg_uuid.clone())],
        }];
        if let Some(lv) = &self.lv_target {
            targets.push(TargetFingerprint {
                planned_id: lv.planned_id,
                external_ids: vec![ExternalId::LvmLvUuid(lv.lv_uuid.clone())],
            });
        }
        if let Some(fs) = &self.filesystem_target {
            targets.push(TargetFingerprint {
                planned_id: fs.planned_id,
                external_ids: vec![ExternalId::Filesystem {
                    fs_type: "ext4".into(),
                    value: fs.uuid.clone(),
                }],
            });
        }
        targets
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
    resolve_action_with_bindings(run, action, observed, backend, &OutputBindings::new(run))
}

/// Resolves the narrow LVM/ext4/Host mount recipe on verified outputs and checks accumulated scope.
/// An output binding never adopts arbitrary live consumers or changes to existing reviewed devices.
pub fn resolve_action_with_bindings(
    run: &ExecutionRun,
    action: ActionId,
    observed: &LiveObservation,
    backend: BackendId,
    bindings: &OutputBindings,
) -> Result<ResolvedAction, TargetError> {
    observed.validate()?;
    let context = run.context();
    let planned = context
        .plan
        .actions()
        .iter()
        .find(|item| item.id == action)
        .ok_or(ExecutionRunError::UnknownAction(action))?;
    let baseline = bindings
        .reviewed_scope(run, observed)
        .map_err(|_| TargetError::UnverifiedOutput)?;
    let mut filesystem_target = None;
    // Reuse the verified LV resolver; this conversion never changes the frozen dispatched operation.
    let endpoint_operation = if let ProviderAction::Mount(MountAction::Mount {
        source: MountSource::Filesystem(fs),
        target,
        ..
    }) = &planned.operation
    {
        let observed_id = bindings
            .resolve(run, *fs, observed)
            .map_err(|_| TargetError::UnverifiedOutput)?;
        if !storage_provider::is_ext4_host_mount(&planned.operation)
            || observed.snapshot.mounts.entries.iter().any(|mount| {
                mount.context == storage_core::model::MountContext::Host
                    && (mount.target.starts_with(target)
                        || mount.source == MountSource::Filesystem(observed_id))
            })
        {
            return Err(TargetError::FilesystemEndpoint);
        }
        let node = baseline
            .graph
            .node(fs)
            .ok_or(TargetError::UnverifiedOutput)?;
        if !matches!(
            node.kind.kind,
            NodeKind::Filesystem {
                kind: FilesystemKind::Ext4,
                ..
            }
        ) {
            return Err(TargetError::UnsupportedAction);
        }
        let mut parents = baseline.graph.dependencies().filter(|edge| {
            edge.to == *fs && edge.kind == storage_core::model::DependencyKind::Backs
        });
        let lv = parents.next().ok_or(TargetError::UnverifiedOutput)?.from;
        if parents.next().is_some() {
            return Err(TargetError::UnverifiedOutput);
        }
        let uuid = node
            .size
            .identities
            .iter()
            .find_map(|id| match id {
                ExternalId::Filesystem { fs_type, value } if fs_type == "ext4" => {
                    Some(value.clone())
                }
                _ => None,
            })
            .ok_or(TargetError::UnverifiedOutput)?;
        filesystem_target = Some(ResolvedFilesystemTarget {
            planned_id: *fs,
            observed_id,
            uuid,
        });
        ProviderAction::Lvm(LvmAction::SetActivation {
            target: lv,
            active: true,
        })
    } else {
        planned.operation.clone()
    };
    let (vg, lv_target) = match &endpoint_operation {
        ProviderAction::Lvm(LvmAction::CreateLv {
            vg,
            layout: LvmLvKind::Linear,
            ..
        }) => (*vg, None),
        ProviderAction::Lvm(LvmAction::SetActivation {
            target,
            active: true,
        })
        | ProviderAction::Filesystem(FilesystemAction::Create {
            backing: target,
            filesystem: FilesystemKind::Ext4,
            ..
        }) => {
            let id = bindings
                .resolve(run, *target, observed)
                .map_err(|_| TargetError::UnverifiedOutput)?;
            let node = observed
                .snapshot
                .graph
                .node(&id)
                .ok_or(TargetError::UnverifiedOutput)?;
            let NodeKind::LvmLv(lv) = &node.kind.kind else {
                return Err(TargetError::UnsupportedAction);
            };
            if lv.kind != LvmLvKind::Linear {
                return Err(TargetError::UnsupportedAction);
            }
            let mut parents = baseline
                .graph
                .dependencies()
                .filter(|edge| edge.to == *target);
            let parent = parents.next().ok_or(TargetError::UnverifiedOutput)?;
            if parents.next().is_some() {
                return Err(TargetError::UnverifiedOutput);
            }
            if let ProviderAction::Filesystem(FilesystemAction::Create { label, .. }) =
                &planned.operation
                && (label
                    .as_ref()
                    .is_some_and(|label| label.len() > 16 || label.contains('\0'))
                    || node.size.lvm_active != Some(true)
                    || !node.size.block.as_ref().is_some_and(|block| {
                        block.read_only == Some(false)
                            && block.geometry.is_some()
                            && !block.paths.is_empty()
                            && block.paths.iter().all(|path| {
                                path.is_absolute()
                                    && !path
                                        .components()
                                        .any(|part| part == std::path::Component::ParentDir)
                                    && !path.as_os_str().as_encoded_bytes().contains(&0)
                            })
                    })
                    || observed
                        .snapshot
                        .graph
                        .dependencies()
                        .any(|edge| edge.from == id)
                    || observed
                        .snapshot
                        .graph
                        .relations()
                        .any(|edge| edge.from == id || edge.to == id))
            {
                return Err(TargetError::FilesystemEndpoint);
            }
            let uuid = node
                .size
                .identities
                .iter()
                .find_map(|identity| match identity {
                    ExternalId::LvmLvUuid(uuid) => Some(uuid.clone()),
                    _ => None,
                })
                .ok_or(TargetError::UnverifiedOutput)?;
            (
                parent.from,
                Some(ResolvedLvTarget {
                    planned_id: *target,
                    observed_id: id,
                    lv_uuid: uuid,
                    lv_name: lv.name.clone(),
                    size: node.kind.size.ok_or(TargetError::UnverifiedOutput)?,
                    block: node.size.block.clone(),
                }),
            )
        }
        _ => return Err(TargetError::UnsupportedAction),
    };
    if filesystem_target.is_some()
        && !lv_target.as_ref().is_some_and(|lv| {
            observed
                .snapshot
                .graph
                .node(&lv.observed_id)
                .is_some_and(|node| node.size.lvm_active == Some(true))
                && lv.block.as_ref().is_some_and(|block| {
                    block.read_only == Some(false)
                        && block.geometry.is_some()
                        && block.paths.len() == 1
                })
        })
    {
        return Err(TargetError::FilesystemEndpoint);
    }
    let mapping = correlate_nodes(&baseline, &observed.snapshot, &[vg])?;
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
    if let ProviderAction::Lvm(LvmAction::CreateLv { name, size, .. }) = &planned.operation {
        // Validate the exact UUID-qualified C endpoint before authorization and checkpoint publication.
        LinearLvRequest::new(vg_name, uuid, name, size.as_u64())?;
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
    }
    if let Some(lv) = &lv_target {
        LvActivationRequest::new(vg_name, uuid, &lv.lv_name, &lv.lv_uuid, lv.size.as_u64())?;
    }
    check_scope(&baseline, &observed.snapshot, vg)?;
    Ok(ResolvedAction {
        run_id: run.run_id(),
        action_id: action,
        operation: planned.operation.clone(),
        backend,
        observation: observed.reference,
        target: ResolvedTarget {
            planned_id: vg,
            observed_id: target.target,
            vg_uuid: uuid.clone(),
            vg_name: vg_name.clone(),
        },
        lv_target,
        filesystem_target,
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
    let mut actual_mounts = current.mounts.entries.clone();
    for entries in [&mounts, &actual_mounts] {
        if entries.iter().enumerate().any(|(index, mount)| {
            entries[..index]
                .iter()
                .any(|other| other.target == mount.target && other.context == mount.context)
        }) {
            return Err(TargetError::ScopeChanged);
        }
    }
    for mount in &mut mounts {
        match &mut mount.source {
            MountSource::Filesystem(id)
            | MountSource::BtrfsSubvolume(id)
            | MountSource::ZfsDataset(id) => {
                // Mounts outside the edited component still need stable source correlation.
                // Equality of unrelated transient IDs could otherwise hide a replacement filesystem.
                let mapping = correlate_nodes(baseline, current, &[*id])?;
                *id = mapping.first().ok_or(TargetError::ScopeChanged)?.target;
            }
            _ => {}
        }
        mount.options.sort_unstable();
    }
    for mount in &mut actual_mounts {
        mount.options.sort_unstable();
    }
    let mut expected: Vec<_> = mounts
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<_, _>>()?;
    let mut actual: Vec<_> = actual_mounts
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
