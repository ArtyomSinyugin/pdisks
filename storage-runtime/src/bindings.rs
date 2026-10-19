//! New LV and ext4 identities become usable only after receipt, fresh observation and verified postconditions.

use crate::targets::{ResolvedAction, TargetError, check_scope};
use std::collections::HashMap;
use storage_core::model::{
    CurrentState, Dependency, DependencyKind, ExternalId, Node, NodeFacts, NodeId, NodeKind,
    NodeSpec, Presence,
};
use storage_engine::{
    ActionId, ActionRunState, ExecutionRun, ExecutionRunError, LiveObservation,
    ObservationReference, VerificationReport,
    editing::NodeCorrelation,
    live::{ObservationError, check_observed_postconditions_with_bindings},
};
use storage_provider::{ActionOutput, FilesystemAction, LvmAction, ProviderAction};
use thiserror::Error;
use uuid::Uuid;

/// Binding failures preserve the active checkpoint and forbid successor dispatch.
#[derive(Debug, Error)]
pub enum BindingError {
    /// A binding belongs to another attempt or publication, or is not yet verified.
    #[error("output binding does not belong to this verified action")]
    NotVerified,
    /// Receipt UUID is absent, ambiguous, preexisting, or points outside the admitted VG.
    #[error("created output identity or ownership is not established by fresh evidence")]
    Identity,
    /// Activation did not establish the contract's actual writable block interface.
    #[error("activation requires a freshly observed writable block interface with known geometry")]
    InterfaceUnknown,
    /// An actual result changed the reviewed scope beyond the new LV.
    #[error(transparent)]
    Scope(#[from] TargetError),
    /// Runtime state cannot be read safely.
    #[error(transparent)]
    Run(#[from] ExecutionRunError),
    /// Actual observed facts are malformed.
    #[error(transparent)]
    Observation(#[from] ObservationError),
}

/// Correlation candidate kept private until all frozen postconditions have passed.
#[derive(Debug)]
pub(crate) struct PendingLvBinding {
    /// Run that actually obtained the post-backend observation.
    run_id: Uuid,
    /// Action whose new output is being correlated.
    action: ActionId,
    /// Exact retained completion publication used for verification.
    observation: ObservationReference,
    /// Node identity reserved in the reviewed plan.
    planned: NodeId,
    /// New UUID returned by the adapter and independently observed.
    uuid: String,
    /// UUID of the existing VG admitted before dispatch.
    vg_uuid: String,
    /// Requested properties independently checked by the engine before publishing this binding.
    spec: NodeSpec,
}

/// A filesystem candidate is private until its actual completion passes all frozen postconditions.
#[derive(Debug)]
pub(crate) struct PendingFilesystemBinding {
    /// Attempt and action that dispatched this filesystem creation.
    run_id: Uuid,
    /// Frozen action that owns this output.
    action: ActionId,
    /// Exact retained post-dispatch publication.
    observation: ObservationReference,
    /// Stable planned filesystem identity.
    planned: NodeId,
    /// New ext4 UUID independently seen in the completion.
    uuid: String,
    /// Verified LV output from the same attempt.
    backing: NodeId,
    /// Requested properties; observed filesystem capacity is not a managed create property.
    spec: NodeSpec,
}

/// Confirmed new outputs scoped to one attempt; no deserialization or manual insertion exists.
#[derive(Debug)]
pub struct OutputBindings {
    /// Attempt that owns every entry.
    run_id: Uuid,
    /// Only candidates that passed the engine's full verification are inserted here.
    entries: HashMap<NodeId, PendingLvBinding>,
    /// Ext4 outputs whose UUID, owner, scope and postconditions were independently verified.
    filesystems: HashMap<NodeId, PendingFilesystemBinding>,
    /// Exact requested mounts whose actual completion passed full verification.
    mounts: Vec<storage_core::model::ObservedMount>,
}

impl OutputBindings {
    /// Starts an empty map for one specific attempt; this grants no execution authorization.
    pub fn new(run: &ExecutionRun) -> Self {
        Self {
            run_id: run.run_id(),
            entries: HashMap::new(),
            filesystems: HashMap::new(),
            mounts: Vec::new(),
        }
    }
    /// Records only a Verified mount whose retained completion has already passed verify_mount.
    pub(crate) fn record_verified_mount(
        &mut self,
        run: &ExecutionRun,
        action: ActionId,
    ) -> Result<(), BindingError> {
        if run.run_id() != self.run_id || run.state(action)? != ActionRunState::Verified {
            return Err(BindingError::NotVerified);
        }
        let operation = &run.context().plan.actions()[action.get() as usize].operation;
        let ProviderAction::Mount(storage_provider::MountAction::Mount {
            source,
            target,
            options,
            context,
        }) = operation
        else {
            return Err(BindingError::Identity);
        };
        self.mounts.push(storage_core::model::ObservedMount {
            source: source.clone(),
            target: target.clone(),
            options: options.clone(),
            context: context.clone(),
        });
        Ok(())
    }

    /// Reports the number of independently verified LV and filesystem outputs.
    pub fn len(&self) -> usize {
        self.entries.len() + self.filesystems.len()
    }
    /// Reports whether this attempt has any verified created outputs.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.filesystems.is_empty()
    }

    /// Resolves a verified output by UUID and its owning VG or backing LV in a fresh observation.
    /// Neither its old transient NodeId nor a matching name suffices after another provider pass.
    pub fn resolve(
        &self,
        run: &ExecutionRun,
        planned: NodeId,
        observed: &LiveObservation,
    ) -> Result<NodeId, BindingError> {
        if self.run_id != run.run_id() {
            return Err(BindingError::NotVerified);
        }
        if let Some(binding) = self.filesystems.get(&planned) {
            observed.validate()?;
            if observed.reference.probe_instance != binding.observation.probe_instance
                || observed.reference.epoch < binding.observation.epoch
            {
                return Err(BindingError::Identity);
            }
            let backing = self.resolve(run, binding.backing, observed)?;
            let NodeKind::Filesystem { label, .. } = &binding.spec.kind else {
                return Err(BindingError::Identity);
            };
            let id = crate::filesystem::observe_ext4(observed, backing, &binding.uuid, label)
                .map_err(|_| BindingError::Identity)?;
            if observed.snapshot.graph.node(&id).map(|node| &node.kind) != Some(&binding.spec) {
                return Err(BindingError::Identity);
            }
            return Ok(id);
        }
        let binding = self
            .entries
            .get(&planned)
            .ok_or(BindingError::NotVerified)?;
        observed.validate()?;
        if observed.reference.probe_instance != binding.observation.probe_instance
            || observed.reference.epoch < binding.observation.epoch
        {
            return Err(BindingError::Identity);
        }
        let id = find_lv(observed, &binding.uuid, &binding.vg_uuid)?;
        if observed.snapshot.graph.node(&id).map(|node| &node.kind) != Some(&binding.spec) {
            return Err(BindingError::Identity);
        }
        Ok(id)
    }

    /// Revalidates every verified UUID against this publication before passing correlations to engine.
    pub(crate) fn correlations(
        &self,
        run: &ExecutionRun,
        observed: &LiveObservation,
    ) -> Result<Vec<NodeCorrelation>, BindingError> {
        if self.run_id != run.run_id() {
            return Err(BindingError::NotVerified);
        }
        self.entries
            .keys()
            .chain(self.filesystems.keys())
            .map(|planned| {
                let identity = if let Some(binding) = self.entries.get(planned) {
                    ExternalId::LvmLvUuid(binding.uuid.clone())
                } else {
                    let binding = self
                        .filesystems
                        .get(planned)
                        .ok_or(BindingError::Identity)?;
                    ExternalId::Filesystem {
                        fs_type: "ext4".into(),
                        value: binding.uuid.clone(),
                    }
                };
                Ok(NodeCorrelation {
                    source: *planned,
                    target: self.resolve(run, *planned, observed)?,
                    identity,
                })
            })
            .collect()
    }

    /// Extends reviewed scope only with independently verified outputs, never with arbitrary live nodes.
    /// Actual topology is compared against these expectations separately; copying live topology would hide drift.
    pub(crate) fn reviewed_scope(
        &self,
        run: &ExecutionRun,
        observed: &LiveObservation,
    ) -> Result<CurrentState, BindingError> {
        self.correlations(run, observed)?;
        let mut expected = run.context().historical_baseline.snapshot.clone();
        for (planned, binding) in &self.entries {
            let mut parents = expected.graph.nodes().filter(|(_, node)| {
                matches!(node.kind.kind, NodeKind::LvmVg { .. })
                    && node
                        .size
                        .identities
                        .contains(&ExternalId::LvmVgUuid(binding.vg_uuid.clone()))
            });
            let parent = *parents.next().ok_or(BindingError::Identity)?.0;
            if parents.next().is_some() {
                return Err(BindingError::Identity);
            }
            drop(parents);
            expected
                .graph
                .insert_new(
                    *planned,
                    Node {
                        kind: binding.spec.clone(),
                        size: NodeFacts {
                            presence: Presence::Present,
                            identities: vec![ExternalId::LvmLvUuid(binding.uuid.clone())],
                            ..NodeFacts::default()
                        },
                    },
                )
                .map_err(|_| BindingError::Identity)?;
            expected.graph.insert_dependency(Dependency {
                from: parent,
                to: *planned,
                kind: DependencyKind::Provides,
            });
        }
        for (planned, binding) in &self.filesystems {
            expected
                .graph
                .insert_new(
                    *planned,
                    Node {
                        kind: binding.spec.clone(),
                        size: NodeFacts {
                            presence: Presence::Present,
                            identities: vec![ExternalId::Filesystem {
                                fs_type: "ext4".into(),
                                value: binding.uuid.clone(),
                            }],
                            ..NodeFacts::default()
                        },
                    },
                )
                .map_err(|_| BindingError::Identity)?;
            expected.graph.insert_dependency(Dependency {
                from: binding.backing,
                to: *planned,
                kind: DependencyKind::Backs,
            });
        }
        expected.mounts.entries.extend(self.mounts.iter().cloned());
        Ok(expected)
    }

    /// Publishes a filesystem only after this exact completion is accepted as Verified.
    pub(crate) fn record_verified_filesystem(
        &mut self,
        run: &ExecutionRun,
        binding: PendingFilesystemBinding,
    ) -> Result<(), BindingError> {
        if self.run_id != run.run_id()
            || binding.run_id != run.run_id()
            || run.state(binding.action)? != ActionRunState::Verified
            || run
                .completion(binding.action)?
                .map(|completion| completion.observation.reference)
                != Some(binding.observation)
            || !self.entries.contains_key(&binding.backing)
            || self.entries.contains_key(&binding.planned)
            || self.filesystems.contains_key(&binding.planned)
            || self
                .filesystems
                .values()
                .any(|other| other.uuid == binding.uuid)
        {
            return Err(BindingError::NotVerified);
        }
        self.filesystems.insert(binding.planned, binding);
        Ok(())
    }

    /// Publishes only a candidate bound to the exact completion just accepted as Verified.
    pub(crate) fn record_verified(
        &mut self,
        run: &ExecutionRun,
        binding: PendingLvBinding,
    ) -> Result<(), BindingError> {
        if self.run_id != run.run_id()
            || binding.run_id != run.run_id()
            || run.state(binding.action)? != ActionRunState::Verified
            || run
                .completion(binding.action)?
                .map(|completion| completion.observation.reference)
                != Some(binding.observation)
            || self.entries.contains_key(&binding.planned)
            || self.filesystems.contains_key(&binding.planned)
            || self
                .entries
                .values()
                .any(|other| other.uuid == binding.uuid)
        {
            return Err(BindingError::NotVerified);
        }
        self.entries.insert(binding.planned, binding);
        Ok(())
    }
}

/// Verifies the retained actual completion, using a provisional UUID mapping only for the new LV.
/// A missing receipt UUID stays unknown; no predicted node is copied into an observation.
pub(crate) fn verify_created_lv(
    run: &ExecutionRun,
    action: ActionId,
    resolved: &ResolvedAction,
    bindings: &OutputBindings,
    before: &LiveObservation,
    created_uuid: Option<&str>,
) -> Result<(VerificationReport, Option<PendingLvBinding>), BindingError> {
    let completion = run.completion(action)?.ok_or(BindingError::NotVerified)?;
    let context = run.context();
    let contract = &context.plan.actions()[action.get() as usize].informational_contract;
    let mut pending = None;
    let mut correlations = Vec::new();
    if let Some(uuid) = created_uuid {
        let ProviderAction::Lvm(LvmAction::CreateLv {
            planned_node_id, ..
        }) = resolved.operation()
        else {
            return Err(BindingError::Identity);
        };
        let identity = ExternalId::LvmLvUuid(uuid.into());
        if before
            .snapshot
            .graph
            .nodes()
            .any(|(_, node)| node.size.identities.contains(&identity))
        {
            return Err(BindingError::Identity);
        }
        let actual = &completion.observation;
        let observed_id = find_lv(actual, uuid, &resolved.target().vg_uuid)?;
        let spec = contract
            .provided_outputs
            .iter()
            .find_map(|output| match output {
                ActionOutput::Node { id, spec } if id == planned_node_id => Some(spec.clone()),
                _ => None,
            })
            .ok_or(BindingError::Identity)?;
        // Remove only the independently identified new node to compare all remaining actual scope.
        let mut remaining = actual.snapshot.clone();
        let edges: Vec<_> = remaining
            .graph
            .dependencies()
            .filter(|edge| edge.from == observed_id || edge.to == observed_id)
            .cloned()
            .collect();
        if edges.iter().any(|edge| edge.from == observed_id)
            || remaining
                .graph
                .relations()
                .any(|edge| edge.from == observed_id || edge.to == observed_id)
        {
            return Err(BindingError::Identity);
        }
        for edge in edges {
            remaining.graph.remove_dependency(&edge);
        }
        remaining.graph.remove_node(&observed_id);
        check_scope(
            &bindings.reviewed_scope(run, before)?,
            &remaining,
            resolved.target().planned_id,
        )?;
        correlations.push(NodeCorrelation {
            source: *planned_node_id,
            target: observed_id,
            identity,
        });
        pending = Some(PendingLvBinding {
            run_id: run.run_id(),
            action,
            observation: actual.reference,
            planned: *planned_node_id,
            uuid: uuid.into(),
            vg_uuid: resolved.target().vg_uuid.clone(),
            spec,
        });
    }
    let report = check_observed_postconditions_with_bindings(
        context.historical_baseline,
        &completion.observation,
        &contract.postconditions,
        &correlations,
    )?;
    Ok((
        VerificationReport {
            run_id: run.run_id(),
            action,
            observation: report.observation,
            checks: report.checks,
        },
        pending,
    ))
}

/// Verifies activation and the promised writable interface without accepting a created-UUID receipt.
pub(crate) fn verify_activation(
    run: &ExecutionRun,
    action: ActionId,
    resolved: &ResolvedAction,
    bindings: &OutputBindings,
    before: &LiveObservation,
    created_uuid: Option<&str>,
) -> Result<VerificationReport, BindingError> {
    if created_uuid.is_some() {
        return Err(BindingError::Identity);
    }
    let completion = run.completion(action)?.ok_or(BindingError::NotVerified)?;
    let actual = &completion.observation;
    check_scope(
        &bindings.reviewed_scope(run, before)?,
        &actual.snapshot,
        resolved.target().planned_id,
    )?;
    let correlations = bindings.correlations(run, actual)?;
    let lv = resolved.lv_target().ok_or(BindingError::Identity)?;
    let id = bindings.resolve(run, lv.planned_id, actual)?;
    let node = actual
        .snapshot
        .graph
        .node(&id)
        .ok_or(BindingError::Identity)?;
    // Activation's contract publishes a read-write block interface; an active flag alone is insufficient.
    if !node.size.block.as_ref().is_some_and(|block| {
        block.read_only == Some(false) && block.geometry.is_some() && !block.paths.is_empty()
    }) {
        return Err(BindingError::InterfaceUnknown);
    }
    let context = run.context();
    let report = check_observed_postconditions_with_bindings(
        context.historical_baseline,
        actual,
        &context.plan.actions()[action.get() as usize]
            .informational_contract
            .postconditions,
        &correlations,
    )?;
    Ok(VerificationReport {
        run_id: run.run_id(),
        action,
        observation: report.observation,
        checks: report.checks,
    })
}

/// Requires one present LV, one UUID, and one Provides edge from the unique admitted VG UUID.
fn find_lv(observed: &LiveObservation, uuid: &str, vg_uuid: &str) -> Result<NodeId, BindingError> {
    if uuid.trim().is_empty() || uuid.trim() != uuid {
        return Err(BindingError::Identity);
    }
    let identity = ExternalId::LvmLvUuid(uuid.into());
    let mut matches = observed
        .snapshot
        .graph
        .nodes()
        .filter(|(_, node)| node.size.identities.contains(&identity));
    let (id, node) = matches.next().ok_or(BindingError::Identity)?;
    if matches.next().is_some()
        || node.size.presence != Presence::Present
        || !matches!(node.kind.kind, NodeKind::LvmLv(_))
        || node
            .size
            .identities
            .iter()
            .filter(|id| matches!(id, ExternalId::LvmLvUuid(_)))
            .count()
            != 1
    {
        return Err(BindingError::Identity);
    }
    let mut parents = observed
        .snapshot
        .graph
        .dependencies()
        .filter(|edge| edge.to == *id);
    let parent = parents.next().ok_or(BindingError::Identity)?;
    if parents.next().is_some() || parent.kind != DependencyKind::Provides {
        return Err(BindingError::Identity);
    }
    let vg = observed
        .snapshot
        .graph
        .node(&parent.from)
        .ok_or(BindingError::Identity)?;
    if !matches!(vg.kind.kind, NodeKind::LvmVg { .. })
        || vg.size.presence != Presence::Present
        || vg
            .size
            .identities
            .iter()
            .filter(|id| matches!(id, ExternalId::LvmVgUuid(_)))
            .count()
            != 1
        || !vg
            .size
            .identities
            .contains(&ExternalId::LvmVgUuid(vg_uuid.into()))
        || observed
            .snapshot
            .graph
            .nodes()
            .filter(|(_, node)| {
                node.size
                    .identities
                    .contains(&ExternalId::LvmVgUuid(vg_uuid.into()))
            })
            .count()
            != 1
    {
        return Err(BindingError::Identity);
    }
    Ok(*id)
}

/// Checks actual ext4 output identity, admitted LV ownership and all remaining reviewed scope.
/// A missing UUID cannot produce a pending binding or a successful NodeMatches postcondition.
pub(crate) fn verify_created_filesystem(
    run: &ExecutionRun,
    action: ActionId,
    resolved: &ResolvedAction,
    bindings: &OutputBindings,
    before: &LiveObservation,
    created_uuid: Option<&str>,
) -> Result<(VerificationReport, Option<PendingFilesystemBinding>), BindingError> {
    let completion = run.completion(action)?.ok_or(BindingError::NotVerified)?;
    let context = run.context();
    let contract = &context.plan.actions()[action.get() as usize].informational_contract;
    let ProviderAction::Filesystem(FilesystemAction::Create {
        backing,
        planned_node_id,
        filesystem: storage_core::model::FilesystemKind::Ext4,
        label,
    }) = resolved.operation()
    else {
        return Err(BindingError::Identity);
    };
    let actual = &completion.observation;
    let mut correlations = bindings.correlations(run, actual)?;
    let mut pending = None;
    if let Some(uuid) = created_uuid {
        let before_backing = bindings.resolve(run, *backing, before)?;
        let binding = crate::filesystem::correlate_created_ext4(
            before,
            actual,
            *planned_node_id,
            before_backing,
            uuid,
            label,
        )
        .map_err(|_| BindingError::Identity)?;
        let mut remaining = actual.snapshot.clone();
        let edges: Vec<_> = remaining
            .graph
            .dependencies()
            .filter(|edge| edge.to == binding.target)
            .cloned()
            .collect();
        for edge in edges {
            remaining.graph.remove_dependency(&edge);
        }
        remaining.graph.remove_node(&binding.target);
        check_scope(
            &bindings.reviewed_scope(run, before)?,
            &remaining,
            resolved.target().planned_id,
        )?;
        let spec = contract
            .provided_outputs
            .iter()
            .find_map(|output| match output {
                ActionOutput::Node { id, spec } if id == planned_node_id => Some(spec.clone()),
                _ => None,
            })
            .ok_or(BindingError::Identity)?;
        let mut spec = spec;
        // Preserve the first actual capacity for later scope checks; creation did not predict it.
        spec.size = actual
            .snapshot
            .graph
            .node(&binding.target)
            .ok_or(BindingError::Identity)?
            .kind
            .size;
        correlations.push(binding);
        pending = Some(PendingFilesystemBinding {
            run_id: run.run_id(),
            action,
            observation: actual.reference,
            planned: *planned_node_id,
            uuid: uuid.into(),
            backing: *backing,
            spec,
        });
    }
    let report = check_observed_postconditions_with_bindings(
        context.historical_baseline,
        actual,
        &contract.postconditions,
        &correlations,
    )?;
    Ok((
        VerificationReport {
            run_id: run.run_id(),
            action,
            observation: report.observation,
            checks: report.checks,
        },
        pending,
    ))
}

/// Verifies exact mount identity/options and all remaining scope from the actual fresh completion.
pub(crate) fn verify_mount(
    run: &ExecutionRun,
    action: ActionId,
    resolved: &ResolvedAction,
    bindings: &OutputBindings,
    before: &LiveObservation,
) -> Result<VerificationReport, BindingError> {
    let completion = run.completion(action)?.ok_or(BindingError::NotVerified)?;
    let actual = &completion.observation;
    let ProviderAction::Mount(storage_provider::MountAction::Mount {
        source: storage_core::model::MountSource::Filesystem(fs),
        target,
        options,
        context,
    }) = resolved.operation()
    else {
        return Err(BindingError::Identity);
    };
    let fs_id = bindings.resolve(run, *fs, actual)?;
    let matches: Vec<_> = actual
        .snapshot
        .mounts
        .entries
        .iter()
        .enumerate()
        .filter(|(_, mount)| &mount.target == target && &mount.context == context)
        .collect();
    let [(index, mount)] = matches.as_slice() else {
        return Err(BindingError::Identity);
    };
    let mut actual_options = mount.options.clone();
    let mut expected_options = options.clone();
    actual_options.sort();
    expected_options.sort();
    if mount.source != storage_core::model::MountSource::Filesystem(fs_id)
        || actual_options != expected_options
    {
        return Err(BindingError::Identity);
    }
    let mut remaining = actual.snapshot.clone();
    remaining.mounts.entries.remove(*index);
    check_scope(
        &bindings.reviewed_scope(run, before)?,
        &remaining,
        resolved.target().planned_id,
    )?;
    let context = run.context();
    let report = check_observed_postconditions_with_bindings(
        context.historical_baseline,
        actual,
        &context.plan.actions()[action.get() as usize]
            .informational_contract
            .postconditions,
        &bindings.correlations(run, actual)?,
    )?;
    Ok(VerificationReport {
        run_id: run.run_id(),
        action,
        observation: report.observation,
        checks: report.checks,
    })
}
