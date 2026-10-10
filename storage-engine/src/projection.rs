//! Pure projection of trusted provider effects.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use storage_core::model::{
    CurrentState, DependencyKind, FilesystemKind, MountContext, MountEntry, Node, NodeFacts,
    NodeGraph, NodeId, NodeKind, ObservedMount,
};
use storage_provider::{
    ActionContract, ActionDescriptionContext, ActionOutput, ActionPrerequisite, AllocationDomain,
    AllocationEffect, AllocationQuantity, BlockInterfaceAccess, GraphEffect, MountEffect,
    NodeKindClass, PredictedEffect, ProviderAction, describe_action,
};
use thiserror::Error;

/// Mutable simulation state derived from an immutable observed snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    graph: NodeGraph,
    runtime_mounts: Vec<ObservedMount>,
    persistent_mounts: Vec<MountEntry>,
    activations: HashMap<NodeId, bool>,
    block_interfaces: HashMap<NodeId, BlockInterfaceAccess>,
    available_capacity: HashMap<AllocationDomain, AllocationQuantity>,
    projected_allocations: HashSet<AllocationDomain>,
    projected_graph_nodes: Vec<NodeId>,
}

impl Projection {
    /// Creates an isolated projection from observed state.
    pub fn from_current(current: &CurrentState) -> Self {
        Self {
            graph: current.graph.clone(),
            runtime_mounts: current.mounts.entries.clone(),
            persistent_mounts: Vec::new(),
            activations: HashMap::new(),
            block_interfaces: HashMap::new(),
            available_capacity: HashMap::new(),
            projected_allocations: HashSet::new(),
            projected_graph_nodes: Vec::new(),
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
        let contract = describe_action(action, ActionDescriptionContext { graph: &self.graph });
        let checks = self.evaluate_prerequisites(&contract.prerequisites);
        if checks
            .iter()
            .any(|check| matches!(check.status, CheckStatus::Violated { .. }))
        {
            return Err(ActionSimulationError::PrerequisitesViolated { checks });
        }
        self.apply_effect(&contract.predicted_effect)?;
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
                    self.runtime_mounts.push(ObservedMount {
                        source: source.clone(),
                        target: target.clone(),
                        options: options.clone(),
                        context: context.clone(),
                    });
                }
                MountEffect::RemoveRuntime { target, context } => {
                    self.runtime_mounts
                        .retain(|mount| mount.target != *target || mount.context != *context);
                }
                MountEffect::SetPersistent(entry) => {
                    self.persistent_mounts.retain(|current| {
                        current.target != entry.target || current.context != entry.context
                    });
                    self.persistent_mounts.push(entry.clone());
                }
                MountEffect::RemovePersistent { target, context } => {
                    self.persistent_mounts
                        .retain(|entry| entry.target != *target || entry.context != *context);
                }
            }
        }
        Ok(())
    }

    /// Records runtime guarantees exported by a successfully projected action.
    fn apply_outputs(&mut self, outputs: &[ActionOutput]) {
        for output in outputs {
            if let ActionOutput::BlockInterface { node, access } = output {
                self.block_interfaces.insert(*node, *access);
            }
        }
    }

    /// Applies cumulative reserve and release effects to known capacity evidence.
    fn apply_allocation_effects(
        &mut self,
        effects: &[AllocationEffect],
    ) -> Result<(), EffectApplicationError> {
        for effect in effects {
            match effect {
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
