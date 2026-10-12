//! Pure projection of trusted provider effects.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use storage_core::model::{
    CurrentState, DependencyKind, Diagnostic, DiagnosticSeverity, FilesystemKind, MountContext,
    MountEntry, MountSource, Node, NodeFacts, NodeGraph, NodeId, NodeKind, ObservedMount,
};
use storage_provider::{
    ActionContract, ActionDescriptionContext, ActionOutput, ActionPrerequisite, AllocationDomain,
    AllocationEffect, AllocationQuantity, BlockInterfaceAccess, FactInvalidation, FactKind,
    FactScope, GraphEffect, MountEffect, NodeKindClass, PredictedEffect, ProviderAction,
    describe_action, validate_provider_action,
};
use thiserror::Error;

/// Mutable simulation state derived from an immutable observed snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    graph: NodeGraph,
    runtime_mounts: Vec<ObservedMount>,
    persistent_mounts: Vec<MountEntry>,
    projected_persistent_targets: Vec<(PathBuf, MountContext)>,
    activations: HashMap<NodeId, bool>,
    block_interfaces: HashMap<NodeId, BlockInterfaceAccess>,
    available_capacity: HashMap<AllocationDomain, AllocationQuantity>,
    projected_allocations: HashSet<AllocationDomain>,
    projected_graph_nodes: Vec<NodeId>,
    /// Conditional clean-check guarantees supplied by preceding forced checks.
    checked_filesystems: HashSet<NodeId>,
    /// Whether runtime mount evidence includes effects rather than only the snapshot.
    projected_runtime_mounts: bool,
}

impl Projection {
    /// Creates an isolated projection from observed state.
    pub fn from_current(current: &CurrentState) -> Self {
        Self {
            graph: current.graph.clone(),
            runtime_mounts: current.mounts.entries.clone(),
            persistent_mounts: Vec::new(),
            projected_persistent_targets: Vec::new(),
            activations: HashMap::new(),
            block_interfaces: HashMap::new(),
            available_capacity: HashMap::new(),
            projected_allocations: HashSet::new(),
            projected_graph_nodes: Vec::new(),
            checked_filesystems: HashSet::new(),
            projected_runtime_mounts: false,
        }
    }

    /// Adds provider evidence about baseline free capacity.
    pub fn with_capacity_evidence(
        mut self,
        evidence: &[CapacityEvidence],
    ) -> Result<Self, CapacityEvidenceError> {
        for item in evidence {
            if self
                .available_capacity
                .insert(item.domain.clone(), item.available)
                .is_some()
            {
                return Err(CapacityEvidenceError::DuplicateDomain(item.domain.clone()));
            }
        }
        Ok(self)
    }

    /// Returns the effective graph after all successfully applied effects.
    pub const fn graph(&self) -> &NodeGraph {
        &self.graph
    }

    /// Returns effective runtime mounts without mixing in persistent entries.
    pub fn runtime_mounts(&self) -> &[ObservedMount] {
        &self.runtime_mounts
    }

    /// Returns persistent entries explicitly established by simulated effects.
    ///
    /// The baseline is unknown because `CurrentState` does not currently carry
    /// persistent mount configuration.
    pub fn persistent_mounts(&self) -> &[MountEntry] {
        &self.persistent_mounts
    }

    /// Reports whether a simulated effect established knowledge about one persistent target.
    pub fn persistent_mount_was_projected(&self, target: &Path, context: &MountContext) -> bool {
        self.projected_persistent_targets
            .iter()
            .any(|candidate| candidate.0 == target && candidate.1 == *context)
    }

    /// Returns a simulated activation request for a node, when one was applied.
    pub fn activation(&self, node: &NodeId) -> Option<bool> {
        self.activations.get(node).copied()
    }

    /// Returns a block interface guaranteed by a simulated predecessor.
    pub fn block_interface(&self, node: &NodeId) -> Option<BlockInterfaceAccess> {
        self.block_interfaces.get(node).copied()
    }

    /// Evaluates action prerequisites against the current read-only projection.
    pub fn evaluate_prerequisites(
        &self,
        prerequisites: &[ActionPrerequisite],
    ) -> Vec<PrerequisiteCheck> {
        prerequisites
            .iter()
            .cloned()
            .map(|prerequisite| PrerequisiteCheck {
                status: self.evaluate_prerequisite(&prerequisite),
                prerequisite,
            })
            .collect()
    }

    /// Describes, checks, and conditionally projects one typed provider action.
    ///
    /// Unknown checks are retained in the result and permit a conditional
    /// projection. A known violation prevents the effect from being applied.
    pub fn simulate_action(
        &mut self,
        action: &ProviderAction,
    ) -> Result<SimulatedAction, ActionSimulationError> {
        let diagnostics = validate_provider_action(action, &self.graph);
        if diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
        {
            return Err(ActionSimulationError::Validation { diagnostics });
        }
        let contract = describe_action(action, ActionDescriptionContext { graph: &self.graph });
        let checks = self.evaluate_prerequisites(&contract.prerequisites);
        if checks
            .iter()
            .any(|check| matches!(check.status, CheckStatus::Violated { .. }))
        {
            return Err(ActionSimulationError::PrerequisitesViolated { checks });
        }
        self.apply_effect(&contract.predicted_effect)?;
        self.invalidate_filesystem_checks(&contract.invalidated_facts);
        self.apply_outputs(&contract.provided_outputs);
        Ok(SimulatedAction { contract, checks })
    }

    /// Atomically applies one trusted predicted effect to this projection.
    pub fn apply_effect(&mut self, effect: &PredictedEffect) -> Result<(), EffectApplicationError> {
        let mut candidate = self.clone();
        candidate.apply_graph_effects(&effect.graph)?;
        candidate.apply_mount_effects(&effect.mounts)?;
        candidate.apply_allocation_effects(&effect.allocations)?;
        candidate.validate_edges()?;
        *self = candidate;
        Ok(())
    }

    /// Evaluates one prerequisite without changing projected state.
    fn evaluate_prerequisite(&self, prerequisite: &ActionPrerequisite) -> CheckStatus {
        match prerequisite {
            ActionPrerequisite::NodeExists(node) => match self.graph.node(node) {
                Some(_) => self.satisfied_for_node(*node),
                None => CheckStatus::Violated {
                    reason: "referenced node is absent".to_owned(),
                },
            },
            ActionPrerequisite::NodeKind { node, expected } => {
                let Some(node_value) = self.graph.node(node) else {
                    return CheckStatus::Violated {
                        reason: "referenced node is absent".to_owned(),
                    };
                };
                if matches_kind(&node_value.kind.kind, *expected) {
                    self.satisfied_for_node(*node)
                } else {
                    CheckStatus::Violated {
                        reason: "node kind does not satisfy the action contract".to_owned(),
                    }
                }
            }
            ActionPrerequisite::NoUnexpectedConsumers(node) => {
                if self.graph.node(node).is_none() {
                    CheckStatus::Violated {
                        reason: "referenced node is absent".to_owned(),
                    }
                } else if self
                    .graph
                    .dependencies()
                    .any(|dependency| dependency.from == *node)
                {
                    CheckStatus::Violated {
                        reason: "node still has an upper-layer consumer".to_owned(),
                    }
                } else {
                    self.satisfied_for_node(*node)
                }
            }
            ActionPrerequisite::Writable(node) => self.evaluate_writable(*node),
            ActionPrerequisite::FilesystemUnmounted(filesystem) => {
                if self.graph.node(filesystem).is_none() {
                    CheckStatus::Violated {
                        reason: "referenced filesystem is absent".to_owned(),
                    }
                } else if self
                    .runtime_mounts
                    .iter()
                    .any(|mount| mount.source == MountSource::Filesystem(*filesystem))
                {
                    CheckStatus::Violated {
                        reason: "filesystem is still mounted in an observed namespace".to_owned(),
                    }
                } else if self.runtime_mounts.iter().any(|mount| {
                    matches!(mount.source, MountSource::Bind(_) | MountSource::Other(_))
                }) {
                    CheckStatus::Unknown { required_evidence: "resolve bind and unsupported mount sources before offline filesystem work".to_owned() }
                } else {
                    CheckStatus::Satisfied(if self.projected_runtime_mounts {
                        CheckEvidence::PredictedEffect
                    } else {
                        CheckEvidence::ObservedSnapshot
                    })
                }
            }
            ActionPrerequisite::FilesystemCheckPassed(filesystem) => {
                if self.checked_filesystems.contains(filesystem) {
                    CheckStatus::Satisfied(CheckEvidence::PredictedEffect)
                } else {
                    CheckStatus::Violated {
                        reason:
                            "filesystem shrink requires a preceding fresh forced read-only check"
                                .to_owned(),
                    }
                }
            }
            ActionPrerequisite::FilesystemMinimumSize { .. } => CheckStatus::Unknown {
                required_evidence:
                    "provider evidence that filesystem content fits the requested shrink target"
                        .to_owned(),
            },
            ActionPrerequisite::DestructiveApproval(_) => CheckStatus::Unknown {
                required_evidence: "explicit destructive-scope approval".to_owned(),
            },
            ActionPrerequisite::ExtentAvailable { .. } => CheckStatus::Unknown {
                required_evidence: "provider allocation and alignment evidence".to_owned(),
            },
            ActionPrerequisite::DistinctNodes(nodes) => {
                let distinct = nodes.iter().copied().collect::<HashSet<_>>();
                if distinct.len() == nodes.len() {
                    CheckStatus::Satisfied(CheckEvidence::PredictedEffect)
                } else {
                    CheckStatus::Violated {
                        reason: "node list contains duplicate identities".to_owned(),
                    }
                }
            }
            ActionPrerequisite::DependentsFitWithin { node, size } => {
                self.evaluate_dependent_sizes(*node, *size)
            }
            ActionPrerequisite::BackingSizeAtLeast { node, size } => {
                self.evaluate_backing_size(*node, *size)
            }
            ActionPrerequisite::CapacityAvailable { domain, amount } => {
                self.evaluate_capacity(domain, *amount)
            }
            ActionPrerequisite::MountTargetAvailable { target, context } => {
                if self.mount_at(target, context).is_some() {
                    CheckStatus::Violated {
                        reason: "mount target is already occupied".to_owned(),
                    }
                } else {
                    CheckStatus::Satisfied(CheckEvidence::ObservedSnapshot)
                }
            }
        }
    }

    /// Resolves writable evidence without turning missing facts into success.
    fn evaluate_writable(&self, node: NodeId) -> CheckStatus {
        let Some(node_value) = self.graph.node(&node) else {
            return CheckStatus::Violated {
                reason: "referenced node is absent".to_owned(),
            };
        };
        // An explicit deactivation supersedes endpoint observations from the baseline.
        if self.activations.get(&node) == Some(&false) {
            return CheckStatus::Violated {
                reason: "projected block endpoint is inactive".to_owned(),
            };
        }
        match self.block_interfaces.get(&node) {
            Some(BlockInterfaceAccess::ReadWrite) => {
                return CheckStatus::Satisfied(CheckEvidence::PredictedEffect);
            }
            Some(BlockInterfaceAccess::ReadOnly) => {
                return CheckStatus::Violated {
                    reason: "projected block interface is read-only".to_owned(),
                };
            }
            None => {}
        }
        match node_value
            .size
            .block
            .as_ref()
            .and_then(|block| block.read_only)
        {
            Some(false) => self.satisfied_for_node(node),
            Some(true) => CheckStatus::Violated {
                reason: "observed block endpoint is read-only".to_owned(),
            },
            None => CheckStatus::Unknown {
                required_evidence: "writable block endpoint observation".to_owned(),
            },
        }
    }

    /// Checks direct block consumers against a proposed lower-layer capacity.
    fn evaluate_dependent_sizes(
        &self,
        node: NodeId,
        size: storage_core::model::Bytes,
    ) -> CheckStatus {
        if self.graph.node(&node).is_none() {
            return CheckStatus::Violated {
                reason: "referenced node is absent".to_owned(),
            };
        }
        let mut found = false;
        let mut predicted = self.projected_graph_nodes.contains(&node);
        for dependency in self.graph.dependencies().filter(|dependency| {
            dependency.from == node && matches!(dependency.kind, DependencyKind::Backs)
        }) {
            found = true;
            predicted |= self.projected_graph_nodes.contains(&dependency.to);
            let Some(dependent) = self.graph.node(&dependency.to) else {
                return CheckStatus::Violated {
                    reason: "dependent node is absent".to_owned(),
                };
            };
            let Some(dependent_size) = dependent.kind.size else {
                return CheckStatus::Unknown {
                    required_evidence: "dependent capacity observation".to_owned(),
                };
            };
            if dependent_size > size {
                return CheckStatus::Violated {
                    reason: "an upper layer exceeds the proposed backing capacity".to_owned(),
                };
            }
        }
        if found {
            CheckStatus::Satisfied(if predicted {
                CheckEvidence::PredictedEffect
            } else {
                CheckEvidence::ObservedSnapshot
            })
        } else {
            self.satisfied_for_node(node)
        }
    }

    /// Checks the direct block backing against a proposed upper-layer capacity.
    fn evaluate_backing_size(&self, node: NodeId, size: storage_core::model::Bytes) -> CheckStatus {
        if self.graph.node(&node).is_none() {
            return CheckStatus::Violated {
                reason: "referenced node is absent".to_owned(),
            };
        }
        let Some(dependency) = self.graph.dependencies().find(|dependency| {
            dependency.to == node && matches!(dependency.kind, DependencyKind::Backs)
        }) else {
            return CheckStatus::Unknown {
                required_evidence: "direct backing relationship".to_owned(),
            };
        };
        let Some(backing) = self.graph.node(&dependency.from) else {
            return CheckStatus::Violated {
                reason: "backing node is absent".to_owned(),
            };
        };
        let Some(backing_size) = backing.kind.size else {
            return CheckStatus::Unknown {
                required_evidence: "backing capacity observation".to_owned(),
            };
        };
        if backing_size < size {
            CheckStatus::Violated {
                reason: "backing capacity is smaller than the requested size".to_owned(),
            }
        } else {
            CheckStatus::Satisfied(
                if self.projected_graph_nodes.contains(&node)
                    || self.projected_graph_nodes.contains(&dependency.from)
                {
                    CheckEvidence::PredictedEffect
                } else {
                    CheckEvidence::ObservedSnapshot
                },
            )
        }
    }

    /// Checks cumulative free capacity in one provider allocation domain.
    fn evaluate_capacity(
        &self,
        domain: &AllocationDomain,
        amount: Option<AllocationQuantity>,
    ) -> CheckStatus {
        let Some(amount) = amount else {
            return CheckStatus::Unknown {
                required_evidence: "provider allocation calculation for this layout".to_owned(),
            };
        };
        let Some(available) = self.available_capacity.get(domain) else {
            return CheckStatus::Unknown {
                required_evidence: "provider allocation-capacity evidence".to_owned(),
            };
        };
        if *available < amount {
            CheckStatus::Violated {
                reason: "allocation domain has insufficient free capacity".to_owned(),
            }
        } else if self.projected_allocations.contains(domain) {
            CheckStatus::Satisfied(CheckEvidence::PredictedEffect)
        } else {
            CheckStatus::Satisfied(CheckEvidence::ObservedSnapshot)
        }
    }

    /// Identifies whether a satisfied fact came from the snapshot or projection.
    fn satisfied_for_node(&self, node: NodeId) -> CheckStatus {
        let evidence = if self.projected_graph_nodes.contains(&node) {
            CheckEvidence::PredictedEffect
        } else {
            CheckEvidence::ObservedSnapshot
        };
        CheckStatus::Satisfied(evidence)
    }

    /// Applies graph changes to an unpublished candidate projection.
    fn apply_graph_effects(
        &mut self,
        effects: &[GraphEffect],
    ) -> Result<(), EffectApplicationError> {
        for effect in effects {
            match effect {
                GraphEffect::InsertNode { id, spec } => {
                    if self.graph.node(id).is_some() {
                        return Err(EffectApplicationError::DuplicateNode(*id));
                    }
                    self.graph.insert_node(
                        *id,
                        Node {
                            kind: spec.clone(),
                            size: NodeFacts::default(),
                        },
                    );
                    self.projected_graph_nodes.push(*id);
                }
                GraphEffect::RemoveNode(node) => {
                    if self.graph.remove_node(node).is_none() {
                        return Err(EffectApplicationError::MissingNode(*node));
                    }
                    self.projected_graph_nodes
                        .retain(|candidate| candidate != node);
                    self.activations.remove(node);
                    self.block_interfaces.remove(node);
                }
                GraphEffect::SetNodeSize { node, size } => {
                    self.update_node(*node, "set node size", |value| {
                        value.kind.size = Some(*size);
                        true
                    })?;
                }
                GraphEffect::SetPartitionAttributes {
                    partition,
                    attributes,
                } => {
                    self.update_node(*partition, "set partition attributes", |value| {
                        let NodeKind::Partition {
                            attributes: current,
                            ..
                        } = &mut value.kind.kind
                        else {
                            return false;
                        };
                        *current = *attributes;
                        true
                    })?;
                }
                GraphEffect::SetFilesystemLabel { filesystem, label } => {
                    self.update_node(*filesystem, "set filesystem label", |value| {
                        let NodeKind::Filesystem { label: current, .. } = &mut value.kind.kind
                        else {
                            return false;
                        };
                        *current = label.clone();
                        true
                    })?;
                }
                GraphEffect::SetBtrfsAllocation {
                    filesystem,
                    allocation,
                } => {
                    self.update_node(*filesystem, "set Btrfs allocation", |value| {
                        let NodeKind::Filesystem {
                            kind: FilesystemKind::Btrfs(current),
                            ..
                        } = &mut value.kind.kind
                        else {
                            return false;
                        };
                        *current = allocation.clone();
                        true
                    })?;
                }
                GraphEffect::InsertDependency(dependency) => {
                    self.graph.insert_dependency(dependency.clone());
                }
                GraphEffect::RemoveDependency(dependency) => {
                    if !self.graph.remove_dependency(dependency) {
                        return Err(EffectApplicationError::MissingDependency);
                    }
                }
                GraphEffect::InsertRelation(relation) => {
                    self.graph.insert_relation(relation.clone());
                }
                GraphEffect::RemoveRelation(relation) => {
                    if !self.graph.remove_relation(relation) {
                        return Err(EffectApplicationError::MissingRelation);
                    }
                }
                GraphEffect::SetActivation { node, active } => {
                    if self.graph.node(node).is_none() {
                        return Err(EffectApplicationError::MissingNode(*node));
                    }
                    self.activations.insert(*node, *active);
                    if !active {
                        self.block_interfaces.remove(node);
                    }
                }
            }
        }
        Ok(())
    }

    /// Applies mount changes without conflating runtime and persistent state.
    fn apply_mount_effects(
        &mut self,
        effects: &[MountEffect],
    ) -> Result<(), EffectApplicationError> {
        for effect in effects {
            match effect {
                MountEffect::AddRuntime {
                    source,
                    target,
                    options,
                    context,
                } => {
                    if self.mount_at(target, context).is_some() {
                        return Err(EffectApplicationError::OccupiedMountTarget(target.clone()));
                    }
                    if let MountSource::Filesystem(filesystem) = source {
                        self.checked_filesystems.remove(filesystem);
                    } else if matches!(source, MountSource::Bind(_) | MountSource::Other(_)) {
                        self.checked_filesystems.clear();
                    }
                    self.projected_runtime_mounts = true;
                    self.runtime_mounts.push(ObservedMount {
                        source: source.clone(),
                        target: target.clone(),
                        options: options.clone(),
                        context: context.clone(),
                    });
                }
                MountEffect::RemoveRuntime { target, context } => {
                    self.projected_runtime_mounts = true;
                    self.runtime_mounts
                        .retain(|mount| mount.target != *target || mount.context != *context);
                }
                MountEffect::SetPersistent(entry) => {
                    self.persistent_mounts.retain(|current| {
                        current.target != entry.target || current.context != entry.context
                    });
                    self.persistent_mounts.push(entry.clone());
                    self.mark_persistent_target(&entry.target, &entry.context);
                }
                MountEffect::RemovePersistent { target, context } => {
                    self.persistent_mounts
                        .retain(|entry| entry.target != *target || entry.context != *context);
                    self.mark_persistent_target(target, context);
                }
            }
        }
        Ok(())
    }

    /// Marks a persistent mount target as determined by a predicted effect.
    fn mark_persistent_target(&mut self, target: &Path, context: &MountContext) {
        if !self.persistent_mount_was_projected(target, context) {
            self.projected_persistent_targets
                .push((target.to_path_buf(), context.clone()));
        }
    }

    /// Records runtime guarantees exported by a successfully projected action.
    fn apply_outputs(&mut self, outputs: &[ActionOutput]) {
        for output in outputs {
            match output {
                ActionOutput::BlockInterface { node, access } => {
                    self.block_interfaces.insert(*node, *access);
                }
                ActionOutput::FilesystemCheckPassed(filesystem) => {
                    self.checked_filesystems.insert(*filesystem);
                }
                _ => {}
            }
        }
    }

    /// Revokes clean-check guarantees when content or backing geometry changes.
    fn invalidate_filesystem_checks(&mut self, invalidations: &[FactInvalidation]) {
        let mut affected = invalidations
            .iter()
            .filter_map(
                |invalidation| match (&invalidation.scope, invalidation.kind) {
                    (
                        FactScope::Node(node),
                        FactKind::Content | FactKind::Geometry | FactKind::Presence,
                    ) => Some(*node),
                    _ => None,
                },
            )
            .collect::<Vec<_>>();
        let mut visited = HashSet::new();
        while let Some(node) = affected.pop() {
            if !visited.insert(node) {
                continue;
            }
            self.checked_filesystems.remove(&node);
            affected.extend(
                self.graph
                    .dependencies()
                    .filter(|dependency| {
                        dependency.from == node
                            && matches!(
                                dependency.kind,
                                DependencyKind::Backs | DependencyKind::Contains { .. }
                            )
                    })
                    .map(|dependency| dependency.to),
            );
        }
    }

    /// Applies cumulative reserve and release effects to known capacity evidence.
    fn apply_allocation_effects(
        &mut self,
        effects: &[AllocationEffect],
    ) -> Result<(), EffectApplicationError> {
        for effect in effects {
            match effect {
                AllocationEffect::Invalidate { domain } => {
                    self.available_capacity.remove(domain);
                    self.projected_allocations.insert(domain.clone());
                }
                AllocationEffect::Reserve { domain, amount } => {
                    let Some(available) = self.available_capacity.get_mut(domain) else {
                        continue;
                    };
                    let Some(remaining) = available.get().checked_sub(amount.get()) else {
                        return Err(EffectApplicationError::InsufficientCapacity(domain.clone()));
                    };
                    *available = AllocationQuantity::new(remaining);
                    self.projected_allocations.insert(domain.clone());
                }
                AllocationEffect::Release { domain, amount } => {
                    let Some(available) = self.available_capacity.get_mut(domain) else {
                        continue;
                    };
                    let Some(total) = available.get().checked_add(amount.get()) else {
                        return Err(EffectApplicationError::CapacityOverflow(domain.clone()));
                    };
                    *available = AllocationQuantity::new(total);
                    self.projected_allocations.insert(domain.clone());
                }
            }
        }
        Ok(())
    }

    /// Replaces one node only after a type-specific update succeeds.
    fn update_node(
        &mut self,
        id: NodeId,
        effect: &'static str,
        update: impl FnOnce(&mut Node) -> bool,
    ) -> Result<(), EffectApplicationError> {
        let Some(mut node) = self.graph.node(&id).cloned() else {
            return Err(EffectApplicationError::MissingNode(id));
        };
        if !update(&mut node) {
            return Err(EffectApplicationError::WrongNodeKind { node: id, effect });
        }
        self.graph.insert_node(id, node);
        if !self.projected_graph_nodes.contains(&id) {
            self.projected_graph_nodes.push(id);
        }
        Ok(())
    }

    /// Finds an effective runtime mount by namespace and target.
    fn mount_at(&self, target: &PathBuf, context: &MountContext) -> Option<&ObservedMount> {
        self.runtime_mounts
            .iter()
            .find(|mount| mount.target == *target && mount.context == *context)
    }

    /// Rejects a candidate graph that would publish dangling edges.
    fn validate_edges(&self) -> Result<(), EffectApplicationError> {
        if self.graph.dependencies().any(|dependency| {
            self.graph.node(&dependency.from).is_none() || self.graph.node(&dependency.to).is_none()
        }) {
            return Err(EffectApplicationError::DanglingDependency);
        }
        if self.graph.relations().any(|relation| {
            self.graph.node(&relation.from).is_none() || self.graph.node(&relation.to).is_none()
        }) {
            return Err(EffectApplicationError::DanglingRelation);
        }
        Ok(())
    }
}

/// Origin of evidence used to satisfy a prerequisite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckEvidence {
    /// Fact copied from the observed baseline snapshot.
    ObservedSnapshot,
    /// Guarantee produced by an earlier simulated effect.
    PredictedEffect,
}

/// Three-valued result of evaluating an action prerequisite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    /// The condition is established by the named evidence source.
    Satisfied(CheckEvidence),
    /// The condition is known to be false.
    Violated {
        /// Human-readable explanation of the contradiction.
        reason: String,
    },
    /// The condition needs evidence unavailable to pure simulation.
    Unknown {
        /// Evidence that a later stage must obtain.
        required_evidence: String,
    },
}

/// Evaluation result tied to the exact prerequisite that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrerequisiteCheck {
    /// Evaluated contract condition.
    pub prerequisite: ActionPrerequisite,
    /// Current result and its evidence or diagnostic.
    pub status: CheckStatus,
}

/// Read-only provider evidence about one allocation domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityEvidence {
    /// Domain to which the observation applies.
    pub domain: AllocationDomain,
    /// Free capacity observed for the planning baseline.
    pub available: AllocationQuantity,
}

/// Invalid set of provider capacity evidence.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CapacityEvidenceError {
    /// More than one observation was supplied for one domain.
    #[error("duplicate capacity evidence for allocation domain {0:?}")]
    DuplicateDomain(AllocationDomain),
}

/// Trusted contract and checks produced while simulating one action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedAction {
    /// Contract recalculated against the projection before the action.
    pub contract: ActionContract,
    /// Results of evaluating every prerequisite in that contract.
    pub checks: Vec<PrerequisiteCheck>,
}

impl SimulatedAction {
    /// Reports whether the projection depends on evidence deferred to runtime.
    pub fn is_conditional(&self) -> bool {
        self.checks
            .iter()
            .any(|check| matches!(check.status, CheckStatus::Unknown { .. }))
    }
}

/// Failure to simulate one typed provider action.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ActionSimulationError {
    /// Technology-specific validation rejected the action before effect application.
    #[error("action failed technology-specific validation")]
    Validation {
        /// Provider diagnostics describing the invalid parameters or context.
        diagnostics: Vec<Diagnostic>,
    },
    /// At least one trusted contract prerequisite is known to be false.
    #[error("action has violated prerequisites")]
    PrerequisitesViolated {
        /// Complete check report, including satisfied and unknown conditions.
        checks: Vec<PrerequisiteCheck>,
    },
    /// The trusted effect could not be applied to the projection.
    #[error(transparent)]
    Effect(#[from] EffectApplicationError),
}

/// Failure to apply a predicted effect to an isolated projection.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EffectApplicationError {
    /// An inserted planned identity already exists.
    #[error("planned node {0:?} already exists")]
    DuplicateNode(NodeId),
    /// A graph mutation references an absent node.
    #[error("graph effect references absent node {0:?}")]
    MissingNode(NodeId),
    /// A type-specific mutation references an incompatible node kind.
    #[error("cannot {effect} on node {node:?} with its current kind")]
    WrongNodeKind {
        /// Node rejected by the mutation.
        node: NodeId,
        /// Mutation whose kind requirement failed.
        effect: &'static str,
    },
    /// A requested dependency removal did not match the graph.
    #[error("dependency selected for removal is absent")]
    MissingDependency,
    /// A requested relation removal did not match the graph.
    #[error("relation selected for removal is absent")]
    MissingRelation,
    /// The completed effect would leave a dependency referencing an absent node.
    #[error("predicted effect would leave a dangling dependency")]
    DanglingDependency,
    /// The completed effect would leave a relation referencing an absent node.
    #[error("predicted effect would leave a dangling relation")]
    DanglingRelation,
    /// A runtime mount effect targets an occupied namespace path.
    #[error("runtime mount target {0:?} is already occupied")]
    OccupiedMountTarget(PathBuf),
    /// A reserve effect exceeds known free capacity.
    #[error("insufficient capacity in allocation domain {0:?}")]
    InsufficientCapacity(AllocationDomain),
    /// A release effect overflows the capacity representation.
    #[error("capacity overflow in allocation domain {0:?}")]
    CapacityOverflow(AllocationDomain),
}

/// Reports whether a node kind satisfies a coarse provider contract class.
fn matches_kind(kind: &NodeKind, expected: NodeKindClass) -> bool {
    match expected {
        NodeKindClass::Block => matches!(
            kind,
            NodeKind::Disk
                | NodeKind::NvmeNamespace(_)
                | NodeKind::Loop { .. }
                | NodeKind::Zram
                | NodeKind::DmMultipath
                | NodeKind::Partition { .. }
                | NodeKind::LuksContainer { .. }
                | NodeKind::DmCryptMapping { .. }
                | NodeKind::DmIntegrity { .. }
                | NodeKind::DmVerity { .. }
                | NodeKind::MdArray(_)
                | NodeKind::LvmPv
                | NodeKind::LvmLv(_)
                | NodeKind::Bcache { .. }
                | NodeKind::DmCache { .. }
                | NodeKind::DmWritecache { .. }
                | NodeKind::ZfsVolume { .. }
                | NodeKind::Drbd { .. }
        ),
        NodeKindClass::PartitionTable => matches!(kind, NodeKind::PartitionTable(_)),
        NodeKindClass::Partition => matches!(kind, NodeKind::Partition { .. }),
        NodeKindClass::LuksContainer => matches!(kind, NodeKind::LuksContainer { .. }),
        NodeKindClass::DmCryptMapping => matches!(kind, NodeKind::DmCryptMapping { .. }),
        NodeKindClass::LvmPv => matches!(kind, NodeKind::LvmPv),
        NodeKindClass::LvmVg => matches!(kind, NodeKind::LvmVg { .. }),
        NodeKindClass::LvmLv => matches!(kind, NodeKind::LvmLv(_)),
        NodeKindClass::Filesystem => matches!(kind, NodeKind::Filesystem { .. }),
        NodeKindClass::BtrfsFilesystem => matches!(
            kind,
            NodeKind::Filesystem {
                kind: FilesystemKind::Btrfs(_),
                ..
            }
        ),
    }
}

#[cfg(test)]
mod tests {
    use storage_core::model::{
        Bytes, Diagnostic, Environment, FilesystemKind, LvmLv, LvmLvKind, MountSource, MountState,
        NodeSpec, SystemEnvironment,
    };
    use storage_provider::{FilesystemAction, LvmAction, ProviderAction};

    use super::*;

    /// Invalid LUKS names fail before any graph or runtime effect is published.
    #[test]
    fn invalid_luks_mapping_names_leave_projection_unchanged() {
        let (mut current, _) = current_with_disk();
        let container = NodeId::new();
        current.graph.insert_node(
            container,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::LuksContainer {
                        version: storage_core::model::LuksVersion::Luks2,
                    },
                    size: None,
                },
                size: NodeFacts::default(),
            },
        );
        for name in ["", "bad/name", "bad\0name"] {
            let mut projection = Projection::from_current(&current);
            let original = projection.clone();
            let result = projection.simulate_action(&ProviderAction::Luks(
                storage_provider::LuksAction::Open {
                    container,
                    planned_node_id: NodeId::new(),
                    mapping_name: name.to_owned(),
                    credential: storage_provider::CredentialRef::from_uuid(container.as_uuid()),
                    read_only: false,
                },
            ));
            assert!(
                matches!(
                    result,
                    Err(ActionSimulationError::Validation { diagnostics })
                        if diagnostics.iter().any(|diagnostic|
                            diagnostic.code == "luks.open.invalid_mapping_name")
                ),
                "invalid mapping name accepted: {name:?}"
            );
            assert_eq!(projection, original);
        }
    }

    /// Builds an unmounted filesystem with one known backing device.
    fn current_with_filesystem(kind: FilesystemKind) -> (CurrentState, NodeId, NodeId) {
        let (mut current, disk) = current_with_disk();
        let filesystem = NodeId::new();
        current.graph.insert_node(
            filesystem,
            Node {
                kind: storage_core::model::NodeSpec {
                    kind: NodeKind::Filesystem { kind, label: None },
                    size: Some(storage_core::model::Bytes::new(100)),
                },
                size: NodeFacts::default(),
            },
        );
        current
            .graph
            .insert_dependency(storage_core::model::Dependency {
                from: disk,
                to: filesystem,
                kind: DependencyKind::Backs,
            });
        (current, disk, filesystem)
    }

    /// Mounted ext4 shrink is rejected before publishing a smaller filesystem.
    #[test]
    fn mounted_filesystem_shrink_leaves_projection_unchanged() {
        let (mut current, _, filesystem) = current_with_filesystem(FilesystemKind::Ext4);
        current.mounts.entries.push(ObservedMount {
            source: storage_core::model::MountSource::Filesystem(filesystem),
            target: PathBuf::from("/data"),
            options: vec!["rw".to_owned()],
            context: MountContext::Host,
        });
        let mut projection = Projection::from_current(&current);
        let original = projection.clone();
        let result =
            projection.simulate_action(&ProviderAction::Filesystem(FilesystemAction::Resize {
                filesystem,
                new_size: storage_core::model::Bytes::new(60),
            }));
        assert!(result.is_err());
        assert_eq!(projection, original);
    }

    /// Offline preparation is required in all namespaces and remains a conditional forecast.
    #[test]
    fn shrink_requires_all_unmounts_and_a_forced_clean_check() {
        let (mut current, _, filesystem) = current_with_filesystem(FilesystemKind::Ext4);
        let contexts = [
            MountContext::Host,
            MountContext::Namespace("installer".to_owned()),
        ];
        for context in &contexts {
            current.mounts.entries.push(ObservedMount {
                source: MountSource::Filesystem(filesystem),
                target: PathBuf::from("/data"),
                options: vec!["rw".to_owned()],
                context: context.clone(),
            });
        }
        let baseline = current.clone();
        let mut projection = Projection::from_current(&current);
        let forced_check = ProviderAction::Filesystem(FilesystemAction::Check {
            filesystem,
            force: true,
        });
        let repair = ProviderAction::Filesystem(FilesystemAction::Repair {
            filesystem,
            force: false,
        });
        let resize = ProviderAction::Filesystem(FilesystemAction::Resize {
            filesystem,
            new_size: Bytes::new(60),
        });
        assert!(projection.simulate_action(&forced_check).is_err());
        assert!(projection.simulate_action(&repair).is_err());
        for (index, context) in contexts.into_iter().enumerate() {
            projection
                .simulate_action(&ProviderAction::Mount(
                    storage_provider::MountAction::Unmount {
                        target: PathBuf::from("/data"),
                        context,
                    },
                ))
                .unwrap_or_else(|error| panic!("simulate unmount: {error}"));
            if index == 0 {
                assert!(projection.simulate_action(&resize).is_err());
                assert!(projection.simulate_action(&forced_check).is_err());
            }
        }
        assert!(projection.simulate_action(&resize).is_err());
        projection
            .simulate_action(&ProviderAction::Filesystem(FilesystemAction::Check {
                filesystem,
                force: false,
            }))
            .unwrap_or_else(|error| panic!("simulate non-forced check: {error}"));
        assert!(projection.simulate_action(&resize).is_err());
        let checked = projection
            .simulate_action(&forced_check)
            .unwrap_or_else(|error| panic!("simulate forced check: {error}"));
        assert!(checked.contract.predicted_effect.graph.is_empty());
        assert!(checked.contract.postconditions.contains(
            &storage_provider::Postcondition::FilesystemCheckPassed(filesystem)
        ));
        let resized = projection
            .simulate_action(&resize)
            .unwrap_or_else(|error| panic!("simulate prepared shrink: {error}"));
        assert!(resized.is_conditional());
        assert!(resized.checks.iter().any(|check| matches!(check,
            PrerequisiteCheck { prerequisite: ActionPrerequisite::FilesystemCheckPassed(id), status: CheckStatus::Satisfied(CheckEvidence::PredictedEffect) }
                if *id == filesystem
        )));
        assert!(resized.checks.iter().any(|check| matches!(
            check,
            PrerequisiteCheck {
                prerequisite: ActionPrerequisite::FilesystemMinimumSize { .. },
                status: CheckStatus::Unknown { .. }
            }
        )));
        assert_eq!(current, baseline);
    }

    /// Writes, remounts, and backing geometry changes revoke an earlier clean-check guarantee.
    #[test]
    fn filesystem_check_guarantees_expire_after_changes() {
        let (mut current, disk, filesystem) = current_with_filesystem(FilesystemKind::Ext4);
        let mut backing = current
            .graph
            .node(&disk)
            .cloned()
            .unwrap_or_else(|| panic!("filesystem backing is absent"));
        backing.kind.kind = NodeKind::LvmLv(LvmLv {
            name: "data".to_owned(),
            kind: LvmLvKind::Linear,
        });
        current.graph.insert_node(disk, backing);
        let vg = NodeId::new();
        current.graph.insert_node(
            vg,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::LvmVg {
                        name: "vg0".to_owned(),
                        extent_size: Some(Bytes::new(1)),
                    },
                    size: None,
                },
                size: NodeFacts::default(),
            },
        );
        current
            .graph
            .insert_dependency(storage_core::model::Dependency {
                from: vg,
                to: disk,
                kind: DependencyKind::Provides,
            });
        let forced_check = ProviderAction::Filesystem(FilesystemAction::Check {
            filesystem,
            force: true,
        });
        let mut checked = Projection::from_current(&current);
        checked
            .simulate_action(&forced_check)
            .unwrap_or_else(|error| panic!("simulate forced check: {error}"));
        for action in [
            ProviderAction::Lvm(LvmAction::ResizeLv {
                lv: disk,
                new_size: Bytes::new(2048),
            }),
            ProviderAction::Filesystem(FilesystemAction::Repair {
                filesystem,
                force: false,
            }),
            ProviderAction::Filesystem(FilesystemAction::Resize {
                filesystem,
                new_size: Bytes::new(60),
            }),
            ProviderAction::Mount(storage_provider::MountAction::Mount {
                source: MountSource::Filesystem(filesystem),
                target: PathBuf::from("/data"),
                options: vec!["rw".to_owned()],
                context: MountContext::Host,
            }),
        ] {
            let mut projection = checked.clone();
            projection
                .simulate_action(&action)
                .unwrap_or_else(|error| panic!("simulate check invalidation: {error}"));
            let checks = projection
                .evaluate_prerequisites(&[ActionPrerequisite::FilesystemCheckPassed(filesystem)]);
            assert!(matches!(checks[0].status, CheckStatus::Violated { .. }));
        }
    }

    /// Unsupported recipes and zero capacity are rejected; unknown direction stays conservative.
    #[test]
    fn filesystem_resize_rejects_unsupported_and_unknown_shrink_without_preparation() {
        for kind in [
            FilesystemKind::Xfs,
            FilesystemKind::Other("custom".to_owned()),
        ] {
            let (current, _, filesystem) = current_with_filesystem(kind);
            let mut projection = Projection::from_current(&current);
            assert!(
                matches!(projection.simulate_action(&ProviderAction::Filesystem(FilesystemAction::Resize {
                    filesystem, new_size: Bytes::new(60),
                })), Err(ActionSimulationError::Validation { diagnostics })
                    if diagnostics.iter().any(|diagnostic| diagnostic.code == "filesystem.resize.unsupported")
                )
            );
        }
        let (mut current, _, filesystem) = current_with_filesystem(FilesystemKind::Ext4);
        let mut projection = Projection::from_current(&current);
        assert!(
            matches!(projection.simulate_action(&ProviderAction::Filesystem(FilesystemAction::Resize {
                filesystem, new_size: Bytes::new(0),
            })), Err(ActionSimulationError::Validation { diagnostics })
                if diagnostics.iter().any(|diagnostic| diagnostic.code == "filesystem.resize.zero_size")
            )
        );
        let mut node = current
            .graph
            .node(&filesystem)
            .cloned()
            .unwrap_or_else(|| panic!("filesystem is absent"));
        node.kind.size = None;
        current.graph.insert_node(filesystem, node);
        assert!(
            Projection::from_current(&current)
                .simulate_action(&ProviderAction::Filesystem(FilesystemAction::Resize {
                    filesystem,
                    new_size: Bytes::new(60),
                }))
                .is_err()
        );
    }

    /// Builds a current state containing one backing disk.
    fn current_with_disk() -> (CurrentState, NodeId) {
        let disk = NodeId::new();
        let mut graph = NodeGraph::new();
        graph.insert_node(
            disk,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::Disk,
                    size: Some(Bytes::new(1024)),
                },
                size: NodeFacts::default(),
            },
        );
        (
            CurrentState {
                graph,
                mounts: MountState::default(),
                environment: Environment {
                    host: SystemEnvironment::default(),
                    target: None,
                },
                diagnostics: Vec::<Diagnostic>::new(),
            },
            disk,
        )
    }

    /// A trusted create effect changes only the isolated projection.
    #[test]
    fn filesystem_create_does_not_mutate_current_state() {
        let (current, disk) = current_with_disk();
        let original = current.clone();
        let filesystem = NodeId::new();
        let action = ProviderAction::Filesystem(FilesystemAction::Create {
            backing: disk,
            planned_node_id: filesystem,
            filesystem: FilesystemKind::Ext4,
            label: Some("data".to_owned()),
        });
        let mut projection = Projection::from_current(&current);

        let result = projection
            .simulate_action(&action)
            .unwrap_or_else(|error| panic!("simulate filesystem create: {error}"));
        assert!(result.is_conditional());
        assert_eq!(current, original);
        let projected = projection
            .graph()
            .node(&filesystem)
            .unwrap_or_else(|| panic!("projected filesystem is missing"));
        assert_eq!(projected.size, NodeFacts::default());
        assert!(matches!(
            &projected.kind.kind,
            NodeKind::Filesystem {
                kind: FilesystemKind::Ext4,
                label: Some(label),
            } if label == "data"
        ));
    }

    /// A failed effect leaves the previously published projection unchanged.
    #[test]
    fn effect_application_is_atomic() {
        let (current, _) = current_with_disk();
        let mut projection = Projection::from_current(&current);
        let original = projection.clone();
        let inserted = NodeId::new();
        let missing = NodeId::new();
        let effect = PredictedEffect {
            graph: vec![
                GraphEffect::InsertNode {
                    id: inserted,
                    spec: NodeSpec {
                        kind: NodeKind::Disk,
                        size: None,
                    },
                },
                GraphEffect::SetNodeSize {
                    node: missing,
                    size: Bytes::new(10),
                },
            ],
            mounts: Vec::new(),
            allocations: Vec::new(),
        };

        assert_eq!(
            projection.apply_effect(&effect),
            Err(EffectApplicationError::MissingNode(missing))
        );
        assert_eq!(projection, original);
    }

    /// Runtime and persistent mount effects remain independent.
    #[test]
    fn mount_effects_keep_runtime_and_persistent_state_separate() {
        let (current, disk) = current_with_disk();
        let mut projection = Projection::from_current(&current);
        let target = PathBuf::from("/data");
        let entry = MountEntry {
            source: MountSource::Filesystem(disk),
            target: target.clone(),
            options: vec!["defaults".to_owned()],
            persistent: true,
            context: MountContext::Host,
        };
        let effect = PredictedEffect {
            graph: Vec::new(),
            mounts: vec![MountEffect::SetPersistent(entry.clone())],
            allocations: Vec::new(),
        };

        assert!(projection.apply_effect(&effect).is_ok());
        assert!(projection.runtime_mounts().is_empty());
        assert_eq!(projection.persistent_mounts(), &[entry]);
    }

    /// Missing writable observations remain unknown rather than successful.
    #[test]
    fn writable_requires_observed_evidence() {
        let (current, disk) = current_with_disk();
        let projection = Projection::from_current(&current);
        let checks = projection.evaluate_prerequisites(&[ActionPrerequisite::Writable(disk)]);

        assert!(matches!(
            checks.as_slice(),
            [PrerequisiteCheck {
                status: CheckStatus::Unknown { .. },
                ..
            }]
        ));
    }

    /// A known prerequisite violation prevents projection of the action effect.
    #[test]
    fn violated_prerequisite_blocks_action_effect() {
        let (current, _) = current_with_disk();
        let missing = NodeId::new();
        let action = ProviderAction::Lvm(LvmAction::SetActivation {
            target: missing,
            active: true,
        });
        let mut projection = Projection::from_current(&current);
        let original = projection.clone();

        assert!(matches!(
            projection.simulate_action(&action),
            Err(ActionSimulationError::PrerequisitesViolated { .. })
        ));
        assert_eq!(projection, original);
    }

    /// A verified activation guarantee satisfies a later writable requirement.
    #[test]
    fn activation_provides_projected_writable_evidence() {
        let (mut current, _) = current_with_disk();
        let lv = NodeId::new();
        current.graph.insert_node(
            lv,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::LvmLv(LvmLv {
                        name: "data".to_owned(),
                        kind: LvmLvKind::Linear,
                    }),
                    size: Some(Bytes::new(1024)),
                },
                size: NodeFacts::default(),
            },
        );
        let action = ProviderAction::Lvm(LvmAction::SetActivation {
            target: lv,
            active: true,
        });
        let mut projection = Projection::from_current(&current);

        projection
            .simulate_action(&action)
            .unwrap_or_else(|error| panic!("simulate LV activation: {error}"));
        let checks = projection.evaluate_prerequisites(&[ActionPrerequisite::Writable(lv)]);

        assert_eq!(
            checks[0].status,
            CheckStatus::Satisfied(CheckEvidence::PredictedEffect)
        );
        assert_eq!(
            projection.block_interface(&lv),
            Some(BlockInterfaceAccess::ReadWrite)
        );
    }

    /// Deactivation revokes both predicted and previously observed write access.
    #[test]
    fn deactivation_blocks_filesystem_create_until_reactivated() {
        let (mut current, _) = current_with_disk();
        let lv = NodeId::new();
        current.graph.insert_node(
            lv,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::LvmLv(LvmLv {
                        name: "data".to_owned(),
                        kind: LvmLvKind::Linear,
                    }),
                    size: Some(Bytes::new(1024)),
                },
                size: NodeFacts {
                    block: Some(storage_core::model::BlockFacts {
                        paths: vec![PathBuf::from("/dev/mapper/data")],
                        devno: storage_core::model::DeviceNumber {
                            major: 253,
                            minor: 0,
                        },
                        geometry: None,
                        read_only: Some(false),
                    }),
                    ..NodeFacts::default()
                },
            },
        );
        let baseline = current.clone();
        let mut projection = Projection::from_current(&current);
        for active in [true, false] {
            projection
                .simulate_action(&ProviderAction::Lvm(LvmAction::SetActivation {
                    target: lv,
                    active,
                }))
                .unwrap_or_else(|error| panic!("simulate LV activation {active}: {error}"));
        }
        assert_eq!(projection.activation(&lv), Some(false));
        assert_eq!(projection.block_interface(&lv), None);
        let checks = projection.evaluate_prerequisites(&[ActionPrerequisite::Writable(lv)]);
        assert!(matches!(checks[0].status, CheckStatus::Violated { .. }));

        let create = ProviderAction::Filesystem(FilesystemAction::Create {
            backing: lv,
            planned_node_id: NodeId::new(),
            filesystem: FilesystemKind::Ext4,
            label: None,
        });
        let before_create = projection.clone();
        assert!(matches!(
            projection.simulate_action(&create),
            Err(ActionSimulationError::PrerequisitesViolated { .. })
        ));
        assert_eq!(projection, before_create);

        projection
            .simulate_action(&ProviderAction::Lvm(LvmAction::SetActivation {
                target: lv,
                active: true,
            }))
            .unwrap_or_else(|error| panic!("simulate LV reactivation: {error}"));
        projection.simulate_action(&create).unwrap_or_else(|error| {
            panic!("simulate filesystem create after reactivation: {error}")
        });
        assert_eq!(current, baseline);
    }

    /// A projected read-only interface is a known writable violation.
    #[test]
    fn read_only_output_rejects_writable_requirement() {
        let (current, disk) = current_with_disk();
        let mut projection = Projection::from_current(&current);
        projection.apply_outputs(&[ActionOutput::BlockInterface {
            node: disk,
            access: BlockInterfaceAccess::ReadOnly,
        }]);

        let checks = projection.evaluate_prerequisites(&[ActionPrerequisite::Writable(disk)]);

        assert!(matches!(
            checks.as_slice(),
            [PrerequisiteCheck {
                status: CheckStatus::Violated { .. },
                ..
            }]
        ));
    }

    /// Missing provider capacity evidence remains an explicit unknown.
    #[test]
    fn capacity_is_unknown_without_provider_evidence() {
        let (current, disk) = current_with_disk();
        let projection = Projection::from_current(&current);
        let checks = projection.evaluate_prerequisites(&[ActionPrerequisite::CapacityAvailable {
            domain: AllocationDomain::LvmVolumeGroup(disk),
            amount: Some(AllocationQuantity::new(1)),
        }]);

        assert!(matches!(
            checks.as_slice(),
            [PrerequisiteCheck {
                status: CheckStatus::Unknown { .. },
                ..
            }]
        ));
    }
}
