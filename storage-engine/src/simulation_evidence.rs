//! Temporary simulation guarantees and public evidence summaries.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use storage_core::model::{DependencyKind, DesiredState, Diagnostic, MountContext, NodeId};
use storage_provider::{
    ActionContract, ActionOutput, ActionPrerequisite, AllocationDomain, AllocationEffect,
    AllocationQuantity, BlockInterfaceAccess, FactInvalidation, FactKind, FactScope,
};
use thiserror::Error;

#[cfg(test)]
use crate::{
    effects::{apply_effect, simulate_action},
    prerequisites::evaluate_prerequisites,
};
#[cfg(test)]
use storage_core::model::{CurrentState, MountEntry, NodeGraph};
#[cfg(test)]
use storage_provider::{PredictedEffect, ProviderAction};

/// Temporary checks and resource accounting; configuration belongs exclusively to DesiredState.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SimulationEvidence {
    /// Persistent targets whose contents were explicitly determined by effects.
    pub(crate) projected_persistent_targets: Vec<(PathBuf, MountContext)>,
    /// Runtime activation guarantees, separate from historical node observations.
    pub(crate) activations: HashMap<NodeId, bool>,
    /// Endpoint access supplied by successful predecessor actions.
    pub(crate) block_interfaces: HashMap<NodeId, BlockInterfaceAccess>,
    /// Remaining capacity in allocation domains with known evidence.
    pub(crate) available_capacity: HashMap<AllocationDomain, AllocationQuantity>,
    /// Domains whose capacity was changed or invalidated by effects.
    pub(crate) projected_allocations: HashSet<AllocationDomain>,
    /// Nodes whose semantic properties originate from effects rather than observations.
    pub(crate) projected_graph_nodes: Vec<NodeId>,
    /// Conditional clean-check guarantees supplied by preceding forced checks.
    pub(crate) checked_filesystems: HashSet<NodeId>,
    /// Whether runtime mount evidence includes effects rather than only the snapshot.
    pub(crate) projected_runtime_mounts: bool,
    /// Baseline observation aspects invalidated by successful projected actions.
    pub(crate) invalidated_facts: Vec<FactInvalidation>,
}

impl SimulationEvidence {
    /// Initializes the ledger with unique baseline provider observations.
    pub(crate) fn new(capacity: &[CapacityEvidence]) -> Result<Self, CapacityEvidenceError> {
        let mut evidence = Self::default();
        for item in capacity {
            if evidence
                .available_capacity
                .insert(item.domain.clone(), item.available)
                .is_some()
            {
                return Err(CapacityEvidenceError::DuplicateDomain(item.domain.clone()));
            }
        }
        Ok(evidence)
    }
}

/// Public qualifications of a prediction; excludes internal allocation and runtime guarantees.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimulationEvidenceSummary {
    /// Baseline observations invalidated by planned effects, including activation descendants.
    pub invalidated_facts: Vec<FactInvalidation>,
    /// Persistent mount targets explicitly determined by effects; other targets remain unknown.
    pub determined_persistent_targets: Vec<(PathBuf, MountContext)>,
}

impl From<&SimulationEvidence> for SimulationEvidenceSummary {
    /// Copies only the evidence needed to interpret a returned desired configuration.
    fn from(evidence: &SimulationEvidence) -> Self {
        Self {
            invalidated_facts: evidence.invalidated_facts.clone(),
            determined_persistent_targets: evidence.projected_persistent_targets.clone(),
        }
    }
}

impl SimulationEvidenceSummary {
    /// Reports whether an observation must be refreshed before it can be trusted.
    pub fn is_observation_stale(&self, scope: &FactScope, kind: FactKind) -> bool {
        self.invalidated_facts
            .iter()
            .any(|item| item.scope == *scope && item.kind == kind)
    }

    /// Distinguishes an explicitly predicted persistent target from the unread baseline.
    pub fn persistent_mount_was_projected(&self, target: &Path, context: &MountContext) -> bool {
        self.determined_persistent_targets
            .iter()
            .any(|(path, namespace)| path == target && namespace == context)
    }
}
/// Reports invalidation of an observation aspect; observation availability is checked separately.
pub(crate) fn is_observation_stale(
    evidence: &SimulationEvidence,
    scope: &FactScope,
    kind: FactKind,
) -> bool {
    evidence
        .invalidated_facts
        .iter()
        .any(|invalidation| invalidation.scope == *scope && invalidation.kind == kind)
}

/// Reports whether a simulated effect established knowledge about one persistent target.
fn persistent_mount_was_projected(
    evidence: &SimulationEvidence,
    target: &Path,
    context: &MountContext,
) -> bool {
    evidence
        .projected_persistent_targets
        .iter()
        .any(|candidate| candidate.0 == target && candidate.1 == *context)
}

/// Marks a persistent mount target as determined by a predicted effect.
pub(crate) fn mark_persistent_target(
    evidence: &mut SimulationEvidence,
    target: &Path,
    context: &MountContext,
) {
    if !persistent_mount_was_projected(evidence, target, context) {
        evidence
            .projected_persistent_targets
            .push((target.to_path_buf(), context.clone()));
    }
}

/// Records runtime guarantees exported by a successfully projected action.
pub(crate) fn apply_outputs(evidence: &mut SimulationEvidence, outputs: &[ActionOutput]) {
    for output in outputs {
        match output {
            ActionOutput::BlockInterface { node, access } => {
                evidence.block_interfaces.insert(*node, *access);
            }
            ActionOutput::FilesystemCheckPassed(filesystem) => {
                evidence.checked_filesystems.insert(*filesystem);
            }
            _ => {}
        }
    }
}

/// Records stale baseline aspects and revokes runtime claims affected by activation changes.
pub(crate) fn invalidate_observations(
    desired: &DesiredState,
    evidence: &mut SimulationEvidence,
    invalidations: &[FactInvalidation],
) -> Vec<FactInvalidation> {
    let mut expanded = invalidations.to_vec();
    for invalidation in invalidations {
        if let (FactScope::Node(node), FactKind::Activation) =
            (&invalidation.scope, invalidation.kind)
        {
            let mut pending = vec![*node];
            let mut visited = HashSet::new();
            while let Some(node) = pending.pop() {
                if !visited.insert(node) {
                    continue;
                }
                let propagated = FactInvalidation {
                    scope: FactScope::Node(node),
                    kind: FactKind::Activation,
                };
                if !expanded.contains(&propagated) {
                    expanded.push(propagated);
                }
                let mut children = desired
                    .graph
                    .dependencies()
                    .filter(|dependency| dependency.from == node)
                    .map(|dependency| dependency.to)
                    .collect::<Vec<_>>();
                children.sort_unstable_by_key(|node| node.as_uuid());
                pending.extend(children);
            }
        }
    }
    for invalidation in &expanded {
        if !evidence.invalidated_facts.contains(invalidation) {
            evidence.invalidated_facts.push(invalidation.clone());
        }
        if let (FactScope::Node(node), FactKind::Activation | FactKind::Presence) =
            (&invalidation.scope, invalidation.kind)
        {
            evidence.block_interfaces.remove(node);
            evidence.activations.remove(node);
        }
    }
    expanded
}

/// Revokes clean-check guarantees when content or backing geometry changes.
pub(crate) fn invalidate_filesystem_checks(
    desired: &DesiredState,
    evidence: &mut SimulationEvidence,
    invalidations: &[FactInvalidation],
) {
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
        evidence.checked_filesystems.remove(&node);
        affected.extend(
            desired
                .graph
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
pub(crate) fn apply_allocation_effects(
    evidence: &mut SimulationEvidence,
    effects: &[AllocationEffect],
) -> Result<(), EffectApplicationError> {
    for effect in effects {
        match effect {
            AllocationEffect::Invalidate { domain } => {
                evidence.available_capacity.remove(domain);
                evidence.projected_allocations.insert(domain.clone());
            }
            AllocationEffect::Reserve { domain, amount } => {
                let Some(available) = evidence.available_capacity.get_mut(domain) else {
                    continue;
                };
                let Some(remaining) = available.get().checked_sub(amount.get()) else {
                    return Err(EffectApplicationError::InsufficientCapacity(domain.clone()));
                };
                *available = AllocationQuantity::new(remaining);
                evidence.projected_allocations.insert(domain.clone());
            }
            AllocationEffect::Release { domain, amount } => {
                let Some(available) = evidence.available_capacity.get_mut(domain) else {
                    continue;
                };
                let Some(total) = available.get().checked_add(amount.get()) else {
                    return Err(EffectApplicationError::CapacityOverflow(domain.clone()));
                };
                *available = AllocationQuantity::new(total);
                evidence.projected_allocations.insert(domain.clone());
            }
        }
    }
    Ok(())
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
pub(crate) struct SimulatedAction {
    /// Contract recalculated against DesiredState before the action.
    pub(crate) contract: ActionContract,
    /// Results of evaluating every prerequisite in that contract.
    pub(crate) checks: Vec<PrerequisiteCheck>,
    /// Observations invalidated at this step, including affected activation descendants.
    pub(crate) invalidated_facts: Vec<FactInvalidation>,
}

#[cfg(test)]
impl SimulatedAction {
    /// Reports whether the action depends on evidence deferred to runtime.
    pub(crate) fn is_conditional(&self) -> bool {
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
    /// The trusted effect could not be applied to DesiredState and its evidence.
    #[error(transparent)]
    Effect(#[from] EffectApplicationError),
}

/// Failure to apply a predicted effect atomically to DesiredState and its evidence.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EffectApplicationError {
    /// The completed graph violates a shared representation invariant.
    #[error(transparent)]
    Structure(#[from] storage_core::model::GraphError),
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
    /// A runtime mount effect targets an occupied namespace path.
    #[error("runtime mount target {0:?} is already occupied")]
    OccupiedMountTarget(PathBuf),
    /// A runtime mount change would hide or detach an active descendant of this target.
    #[error("runtime mount target {0:?} still contains an active descendant")]
    MountedDescendant(PathBuf),
    /// A reserve effect exceeds known free capacity.
    #[error("insufficient capacity in allocation domain {0:?}")]
    InsufficientCapacity(AllocationDomain),
    /// A release effect overflows the capacity representation.
    #[error("capacity overflow in allocation domain {0:?}")]
    CapacityOverflow(AllocationDomain),
}

/// Test fixture joining configuration and evidence for existing atomicity scenarios.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TestSimulation {
    /// The fixture's sole future configuration.
    pub(crate) desired: DesiredState,
    /// The fixture's temporary guarantees and resource ledger.
    pub(crate) evidence: SimulationEvidence,
}

#[cfg(test)]
impl TestSimulation {
    /// Starts an isolated simulation fixture.
    pub(crate) fn from_current(current: &CurrentState) -> Self {
        Self {
            desired: DesiredState::from_current(current),
            evidence: SimulationEvidence::default(),
        }
    }
    /// Replaces the fixture's baseline capacity evidence.
    pub(crate) fn with_capacity_evidence(
        mut self,
        capacity: &[CapacityEvidence],
    ) -> Result<Self, CapacityEvidenceError> {
        self.evidence = SimulationEvidence::new(capacity)?;
        Ok(self)
    }
    /// Returns the fixture's single future graph.
    pub(crate) fn graph(&self) -> &NodeGraph {
        &self.desired.graph
    }
    /// Returns future runtime mounts.
    pub(crate) fn runtime_mounts(&self) -> &[MountEntry] {
        &self.desired.mounts.runtime
    }
    /// Returns future persistent entries.
    pub(crate) fn persistent_mounts(&self) -> &[MountEntry] {
        &self.desired.mounts.persistent
    }
    /// Forwards to the shared interpreter for this fixture.
    pub(crate) fn simulate_action(
        &mut self,
        action: &ProviderAction,
    ) -> Result<SimulatedAction, ActionSimulationError> {
        simulate_action(&mut self.desired, &mut self.evidence, action)
    }
    /// Forwards to the shared interpreter for this fixture.
    pub(crate) fn apply_effect(
        &mut self,
        effect: &PredictedEffect,
    ) -> Result<(), EffectApplicationError> {
        apply_effect(&mut self.desired, &mut self.evidence, effect)
    }
    /// Forwards to the shared interpreter for this fixture.
    pub(crate) fn evaluate_prerequisites(
        &self,
        prerequisites: &[ActionPrerequisite],
    ) -> Vec<PrerequisiteCheck> {
        evaluate_prerequisites(&self.desired, &self.evidence, prerequisites)
    }
    /// Forwards to the shared interpreter for this fixture.
    pub(crate) fn is_observation_stale(&self, scope: &FactScope, kind: FactKind) -> bool {
        is_observation_stale(&self.evidence, scope, kind)
    }
    /// Forwards to the shared interpreter for this fixture.
    pub(crate) fn activation(&self, node: &NodeId) -> Option<bool> {
        self.evidence.activations.get(node).copied()
    }
    /// Forwards to the shared interpreter for this fixture.
    pub(crate) fn block_interface(&self, node: &NodeId) -> Option<BlockInterfaceAccess> {
        self.evidence.block_interfaces.get(node).copied()
    }
    /// Forwards to the shared interpreter for this fixture.
    pub(crate) fn apply_outputs(&mut self, outputs: &[ActionOutput]) {
        apply_outputs(&mut self.evidence, outputs)
    }
    /// Forwards to the shared interpreter for this fixture.
    pub(crate) fn invalidated_facts(&self) -> &[FactInvalidation] {
        &self.evidence.invalidated_facts
    }
}
