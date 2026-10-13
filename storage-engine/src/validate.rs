//! Preservation checks for an explicitly protected planning area.

use std::path::{Component, Path, PathBuf};

use storage_core::model::{MountContext, NodeId};
use storage_provider::{ActionResource, FactScope, GraphEffect, MountEffect, ResourceAccessMode};

use crate::{ActionId, ExecutionPlan, PlanSimulationError, SimulationResult};

/// Objects that must remain untouched throughout every predicted transition.
/// This policy restricts planning; it never grants live write permission.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProtectedScope {
    /// Nodes protected against writes, incident edge changes, and mutation-related invalidations.
    pub nodes: Vec<NodeId>,
    /// Runtime targets protected against changes at the target, its ancestors, or descendants.
    pub runtime_mounts: Vec<ProtectedMount>,
    /// Persistent entries protected against configuration writes in their context.
    /// The current provider contract writes a whole context's configuration resource.
    pub persistent_mounts: Vec<ProtectedMount>,
}

/// One namespace-qualified mount target in a preservation policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedMount {
    /// Absolute target whose mount relation is protected.
    pub target: PathBuf,
    /// Namespace or configuration context containing the target.
    pub context: MountContext,
}

/// Protected object touched by a planned change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtectedAspect {
    /// Canonical node, including its contents, runtime state, and incident topology.
    Node(NodeId),
    /// Namespace-qualified runtime mount relation.
    RuntimeMount(ProtectedMount),
    /// Persistent entry in a configuration context.
    PersistentMount(ProtectedMount),
}

/// A planned action that intersects an explicitly protected object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectionViolation {
    /// Action responsible for the change, even if a later action restores the baseline.
    pub action: ActionId,
    /// Protected aspect affected by the action.
    pub aspect: ProtectedAspect,
}

/// Rejects ambiguous paths before they can silently bypass a mount policy.
pub(crate) fn validate_protected_scope(
    protected: &ProtectedScope,
) -> Result<(), PlanSimulationError> {
    for mount in protected
        .runtime_mounts
        .iter()
        .chain(&protected.persistent_mounts)
    {
        let root_valid = match &mount.context {
            MountContext::TargetRoot(root) => valid_policy_path(root),
            MountContext::Host | MountContext::Namespace(_) => true,
        };
        if !valid_policy_path(&mount.target) || !root_valid {
            return Err(PlanSimulationError::InvalidProtectedMount(mount.clone()));
        }
    }
    Ok(())
}

/// Uses the same lexical absolute-path constraints as provider mount actions.
fn valid_policy_path(path: &Path) -> bool {
    path.is_absolute()
        && !path.as_os_str().as_encoded_bytes().contains(&0)
        && !path
            .components()
            .any(|component| component == Component::ParentDir)
}

/// Checks trusted, replayed contracts and expanded invalidations in action order.
pub(crate) fn check_protected_scope(
    plan: &ExecutionPlan,
    simulation: &SimulationResult,
    protected: &ProtectedScope,
) -> Vec<ProtectionViolation> {
    let mut violations = Vec::new();
    for (action, step) in plan.ordered_actions().into_iter().zip(&simulation.steps) {
        let contract = &action.informational_contract;
        // A read-only inspection may refresh content evidence without changing protected data.
        let mutating = contract
            .affected_resources
            .iter()
            .any(|access| access.mode == ResourceAccessMode::Write)
            || !contract.predicted_effect.graph.is_empty()
            || !contract.predicted_effect.mounts.is_empty()
            || !contract.predicted_effect.allocations.is_empty();
        for node in &protected.nodes {
            let written = contract.affected_resources.iter().any(|access| {
                access.mode == ResourceAccessMode::Write
                    && matches!(access.resource, ActionResource::Node(id)
                        | ActionResource::PartitionTable(id) | ActionResource::LvmMetadata(id) if id == *node)
            });
            let invalidated = mutating
                && step
                    .invalidated_facts
                    .iter()
                    .any(|fact| fact.scope == FactScope::Node(*node));
            if written
                || invalidated
                || contract
                    .predicted_effect
                    .graph
                    .iter()
                    .any(|effect| graph_effect_touches(effect, *node))
            {
                push_violation(&mut violations, action.id, ProtectedAspect::Node(*node));
            }
        }
        for mount in &protected.runtime_mounts {
            if contract
                .predicted_effect
                .mounts
                .iter()
                .any(|effect| match effect {
                    MountEffect::AddRuntime {
                        target, context, ..
                    }
                    | MountEffect::RemoveRuntime { target, context } => {
                        *context == mount.context
                            && (target.starts_with(&mount.target)
                                || mount.target.starts_with(target))
                    }
                    MountEffect::SetPersistent(_) | MountEffect::RemovePersistent { .. } => false,
                })
            {
                push_violation(
                    &mut violations,
                    action.id,
                    ProtectedAspect::RuntimeMount(mount.clone()),
                );
            }
        }
        for mount in &protected.persistent_mounts {
            if contract.affected_resources.iter().any(|access| {
                access.mode == ResourceAccessMode::Write
                    && access.resource
                        == ActionResource::PersistentMountConfig(mount.context.clone())
            }) {
                push_violation(
                    &mut violations,
                    action.id,
                    ProtectedAspect::PersistentMount(mount.clone()),
                );
            }
        }
    }
    violations
}

/// Adds one action/aspect conflict once, even if the policy repeats an object.
fn push_violation(
    violations: &mut Vec<ProtectionViolation>,
    action: ActionId,
    aspect: ProtectedAspect,
) {
    let violation = ProtectionViolation { action, aspect };
    if !violations.contains(&violation) {
        violations.push(violation);
    }
}

/// Protects incident topology as well as direct node properties.
fn graph_effect_touches(effect: &GraphEffect, protected: NodeId) -> bool {
    match effect {
        GraphEffect::InsertNode { id, .. }
        | GraphEffect::RemoveNode(id)
        | GraphEffect::InvalidateNodeSize(id) => *id == protected,
        GraphEffect::SetNodeSize { node, .. } | GraphEffect::SetActivation { node, .. } => {
            *node == protected
        }
        GraphEffect::SetPartitionAttributes { partition, .. } => *partition == protected,
        GraphEffect::SetFilesystemLabel { filesystem, .. }
        | GraphEffect::SetBtrfsAllocation { filesystem, .. } => *filesystem == protected,
        GraphEffect::InsertDependency(edge) | GraphEffect::RemoveDependency(edge) => {
            edge.from == protected || edge.to == protected
        }
        GraphEffect::InsertRelation(edge) | GraphEffect::RemoveRelation(edge) => {
            edge.from == protected || edge.to == protected
        }
    }
}
