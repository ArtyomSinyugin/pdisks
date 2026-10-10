//! Composition of provider fragments into one validated action DAG.

use std::collections::{HashMap, HashSet};

use storage_core::model::{CurrentState, DiagnosticSeverity, MountSource, NodeId};
use storage_provider::{
    ActionContract, ActionOutput, ActionPrerequisite, BlockInterfaceAccess, BtrfsAction,
    DependencyReason, FilesystemAction, FragmentOrigin, LocalActionId, LuksAction, LvmAction,
    MountAction, PartitionAction, PlanFragment, ProviderAction,
};
use thiserror::Error;

use crate::{
    ActionDependency, ActionId, ActionSimulationError, ExecutionPlan, FragmentId, PlannedAction,
    Projection,
};

/// Mutable action retained only while a plan is being composed.
#[derive(Debug, Clone)]
struct DraftAction {
    id: ActionId,
    operation: ProviderAction,
    fragment: FragmentId,
    local_id: LocalActionId,
    origin: FragmentOrigin,
}

/// External requirement resolved after the first contract trace.
#[derive(Debug, Clone)]
struct PendingRequirement {
    action: ActionId,
    prerequisite: ActionPrerequisite,
}

/// Structural draft produced before planned inputs and requirements are bound.
struct FlattenedFragments {
    actions: Vec<DraftAction>,
    dependencies: Vec<ActionDependency>,
    requirements: Vec<PendingRequirement>,
}

/// Builds one immutable plan from provider-owned fragments.
pub fn compose_plan(
    current: &CurrentState,
    fragments: Vec<PlanFragment>,
) -> Result<ExecutionPlan, CompositionError> {
    let FlattenedFragments {
        actions: draft,
        mut dependencies,
        requirements,
    } = flatten_fragments(&fragments)?;
    bind_planned_nodes(current, &draft, &mut dependencies)?;

    let first_order = topological_order(draft.len(), &dependencies)?;
    let first_contracts = simulate_order(current, &draft, &first_order)?;
    resolve_requirements(current, &requirements, &first_contracts, &mut dependencies)?;

    let final_order = topological_order(draft.len(), &dependencies)?;
    let final_contracts = simulate_order(current, &draft, &final_order)?;
    validate_declared_requirements(&requirements, &final_contracts)?;

    let actions = draft
        .into_iter()
        .map(|action| {
            let contract = final_contracts
                .get(&action.id)
                .cloned()
                .ok_or(CompositionError::MissingAnalysedContract(action.id))?;
            Ok(PlannedAction {
                id: action.id,
                operation: action.operation,
                fragment: action.fragment,
                local_id: action.local_id,
                origin: action.origin,
                informational_contract: contract,
            })
        })
        .collect::<Result<Vec<_>, CompositionError>>()?;

    Ok(ExecutionPlan::new(actions, dependencies))
}

/// Assigns global identities and preserves fragment-local structure.
fn flatten_fragments(fragments: &[PlanFragment]) -> Result<FlattenedFragments, CompositionError> {
    let mut draft = Vec::new();
    let mut local_to_global = HashMap::new();
    for (fragment_index, fragment) in fragments.iter().enumerate() {
        let fragment_id = FragmentId::new(
            u32::try_from(fragment_index).map_err(|_| CompositionError::PlanTooLarge)?,
        );
        if fragment
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
        {
            return Err(CompositionError::FragmentHasErrors(fragment_id));
        }
        for action in &fragment.actions {
            let id = ActionId::new(
                u32::try_from(draft.len()).map_err(|_| CompositionError::PlanTooLarge)?,
            );
            if local_to_global
                .insert((fragment_index, action.id), id)
                .is_some()
            {
                return Err(CompositionError::DuplicateLocalAction {
                    fragment: fragment_id,
                    action: action.id,
                });
            }
            draft.push(DraftAction {
                id,
                operation: action.operation.clone(),
                fragment: fragment_id,
                local_id: action.id,
                origin: fragment.origin.clone(),
            });
        }
    }

    let mut dependencies = Vec::new();
    let mut requirements = Vec::new();
    for (fragment_index, fragment) in fragments.iter().enumerate() {
        let fragment_id = FragmentId::new(
            u32::try_from(fragment_index).map_err(|_| CompositionError::PlanTooLarge)?,
        );
        for dependency in &fragment.dependencies {
            let before = local_to_global.get(&(fragment_index, dependency.before));
            let after = local_to_global.get(&(fragment_index, dependency.after));
            let (Some(before), Some(after)) = (before, after) else {
                return Err(CompositionError::UnknownLocalDependency(fragment_id));
            };
            add_dependency(&mut dependencies, *before, *after, dependency.reason)?;
        }
        for requirement in &fragment.external_requirements {
            let Some(action) = local_to_global.get(&(fragment_index, requirement.action)) else {
                return Err(CompositionError::UnknownRequirementAction {
                    fragment: fragment_id,
                    action: requirement.action,
                });
            };
            requirements.push(PendingRequirement {
                action: *action,
                prerequisite: requirement.prerequisite.clone(),
            });
        }
    }
    Ok(FlattenedFragments {
        actions: draft,
        dependencies,
        requirements,
    })
}

/// Adds creation-order edges and rejects ambiguous planned identities.
fn bind_planned_nodes(
    current: &CurrentState,
    actions: &[DraftAction],
    dependencies: &mut Vec<ActionDependency>,
) -> Result<(), CompositionError> {
    let mut creators = HashMap::new();
    for action in actions {
        if let Some(node) = planned_node(&action.operation) {
            if current.graph.node(&node).is_some() {
                return Err(CompositionError::PlannedNodeAlreadyExists(node));
            }
            if let Some(first) = creators.insert(node, action.id) {
                return Err(CompositionError::DuplicatePlannedNode {
                    node,
                    first,
                    second: action.id,
                });
            }
        }
    }

    for action in actions {
        for input in referenced_nodes(&action.operation) {
            if current.graph.node(&input).is_some() {
                continue;
            }
            let Some(creator) = creators.get(&input) else {
                return Err(CompositionError::UnresolvedInput {
                    action: action.id,
                    node: input,
                });
            };
            add_dependency(
                dependencies,
                *creator,
                action.id,
                DependencyReason::InputCreation,
            )?;
        }
    }
    Ok(())
}

/// Resolves cross-fragment conditions from baseline evidence or action outputs.
fn resolve_requirements(
    current: &CurrentState,
    requirements: &[PendingRequirement],
    contracts: &HashMap<ActionId, ActionContract>,
    dependencies: &mut Vec<ActionDependency>,
) -> Result<(), CompositionError> {
    let baseline = Projection::from_current(current);
    for requirement in requirements {
        let baseline_check = baseline
            .evaluate_prerequisites(std::slice::from_ref(&requirement.prerequisite))
            .pop()
            .ok_or(CompositionError::MissingRequirementCheck(
                requirement.action,
            ))?;
        if matches!(baseline_check.status, crate::CheckStatus::Satisfied(_)) {
            continue;
        }
        if requirement_can_be_checked_live(&requirement.prerequisite, current) {
            continue;
        }
        let mut providers = contracts
            .iter()
            .filter_map(|(action, contract)| {
                contract
                    .provided_outputs
                    .iter()
                    .any(|output| output_satisfies(output, &requirement.prerequisite))
                    .then_some(*action)
            })
            .collect::<Vec<_>>();
        providers.sort_unstable();
        let Some(provider) = providers.first().copied() else {
            return Err(CompositionError::UnresolvedRequirement {
                action: requirement.action,
            });
        };
        if providers.len() > 1 {
            return Err(CompositionError::AmbiguousRequirement {
                action: requirement.action,
            });
        }
        add_dependency(
            dependencies,
            provider,
            requirement.action,
            DependencyReason::InterfacePreparation,
        )?;
    }
    Ok(())
}

/// Ensures fragment requirements still match trusted final contracts.
fn validate_declared_requirements(
    requirements: &[PendingRequirement],
    contracts: &HashMap<ActionId, ActionContract>,
) -> Result<(), CompositionError> {
    for requirement in requirements {
        let Some(contract) = contracts.get(&requirement.action) else {
            return Err(CompositionError::MissingAnalysedContract(
                requirement.action,
            ));
        };
        if !contract.prerequisites.contains(&requirement.prerequisite) {
            return Err(CompositionError::RequirementNotInContract {
                action: requirement.action,
            });
        }
    }
    Ok(())
}

/// Simulates all actions in one candidate topological order.
fn simulate_order(
    current: &CurrentState,
    actions: &[DraftAction],
    order: &[ActionId],
) -> Result<HashMap<ActionId, ActionContract>, CompositionError> {
    let mut projection = Projection::from_current(current);
    let mut contracts = HashMap::new();
    for id in order {
        let action = &actions[id.get() as usize];
        match projection.simulate_action(&action.operation) {
            Ok(result) => {
                contracts.insert(*id, result.contract);
            }
            Err(ActionSimulationError::PrerequisitesViolated { .. }) => {
                return Err(CompositionError::ActionRejected(*id));
            }
            Err(ActionSimulationError::Effect(error)) => {
                return Err(CompositionError::Effect { action: *id, error });
            }
        }
    }
    Ok(contracts)
}

/// Calculates a stable topological order or reports a cycle.
fn topological_order(
    action_count: usize,
    dependencies: &[ActionDependency],
) -> Result<Vec<ActionId>, CompositionError> {
    let mut indegree = vec![0_usize; action_count];
    for dependency in dependencies {
        indegree[dependency.after.get() as usize] += 1;
    }
    let mut emitted = vec![false; action_count];
    let mut order = Vec::with_capacity(action_count);
    while order.len() < action_count {
        let Some(next) = indegree
            .iter()
            .enumerate()
            .find_map(|(index, degree)| (!emitted[index] && *degree == 0).then_some(index))
        else {
            return Err(CompositionError::DependencyCycle);
        };
        emitted[next] = true;
        order.push(ActionId::new(
            u32::try_from(next).map_err(|_| CompositionError::PlanTooLarge)?,
        ));
        for dependency in dependencies
            .iter()
            .filter(|dependency| dependency.before.get() as usize == next)
        {
            indegree[dependency.after.get() as usize] -= 1;
        }
    }
    Ok(order)
}

/// Adds one non-reflexive dependency while preserving the first reason.
fn add_dependency(
    dependencies: &mut Vec<ActionDependency>,
    before: ActionId,
    after: ActionId,
    reason: DependencyReason,
) -> Result<(), CompositionError> {
    if before == after {
        return Err(CompositionError::SelfDependency(before));
    }
    if !dependencies
        .iter()
        .any(|dependency| dependency.before == before && dependency.after == after)
    {
        dependencies.push(ActionDependency {
            before,
            after,
            reason,
        });
    }
    Ok(())
}

/// Returns the stable node created by an action, when it creates one.
fn planned_node(action: &ProviderAction) -> Option<NodeId> {
    match action {
        ProviderAction::Partition(PartitionAction::CreateTable {
            planned_node_id, ..
        })
        | ProviderAction::Partition(PartitionAction::Create {
            planned_node_id, ..
        })
        | ProviderAction::Luks(LuksAction::Format {
            planned_node_id, ..
        })
        | ProviderAction::Luks(LuksAction::Open {
            planned_node_id, ..
        })
        | ProviderAction::Lvm(LvmAction::CreatePv {
            planned_node_id, ..
        })
        | ProviderAction::Lvm(LvmAction::CreateVg {
            planned_node_id, ..
        })
        | ProviderAction::Lvm(LvmAction::CreateLv {
            planned_node_id, ..
        })
        | ProviderAction::Filesystem(FilesystemAction::Create {
            planned_node_id, ..
        })
        | ProviderAction::Btrfs(BtrfsAction::Create {
            planned_node_id, ..
        })
        | ProviderAction::Btrfs(BtrfsAction::CreateSubvolume {
            planned_node_id, ..
        }) => Some(*planned_node_id),
        _ => None,
    }
}

/// Returns graph nodes structurally referenced as action inputs.
fn referenced_nodes(action: &ProviderAction) -> Vec<NodeId> {
    let mut nodes = match action {
        ProviderAction::Partition(PartitionAction::CreateTable { disk, .. }) => vec![*disk],
        ProviderAction::Partition(PartitionAction::Create { table, .. }) => vec![*table],
        ProviderAction::Partition(PartitionAction::Resize { partition, .. })
        | ProviderAction::Partition(PartitionAction::Delete { partition })
        | ProviderAction::Partition(PartitionAction::SetAttributes { partition, .. }) => {
            vec![*partition]
        }
        ProviderAction::Luks(LuksAction::Format { target, .. }) => vec![*target],
        ProviderAction::Luks(LuksAction::Open { container, .. }) => vec![*container],
        ProviderAction::Luks(LuksAction::Close { mapping })
        | ProviderAction::Luks(LuksAction::Resize { mapping, .. }) => vec![*mapping],
        ProviderAction::Lvm(LvmAction::CreatePv { backing, .. }) => vec![*backing],
        ProviderAction::Lvm(LvmAction::CreateVg { pvs, .. }) => pvs.clone(),
        ProviderAction::Lvm(LvmAction::CreateLv { vg, .. }) => vec![*vg],
        ProviderAction::Lvm(LvmAction::ResizeLv { lv, .. })
        | ProviderAction::Lvm(LvmAction::RemoveLv { lv }) => vec![*lv],
        ProviderAction::Lvm(LvmAction::SetActivation { target, .. }) => vec![*target],
        ProviderAction::Filesystem(FilesystemAction::Create { backing, .. }) => vec![*backing],
        ProviderAction::Filesystem(FilesystemAction::Resize { filesystem, .. })
        | ProviderAction::Filesystem(FilesystemAction::Check { filesystem, .. })
        | ProviderAction::Filesystem(FilesystemAction::Repair { filesystem, .. })
        | ProviderAction::Filesystem(FilesystemAction::SetLabel { filesystem, .. }) => {
            vec![*filesystem]
        }
        ProviderAction::Btrfs(BtrfsAction::Create { members, .. }) => members.clone(),
        ProviderAction::Btrfs(BtrfsAction::AddDevice { filesystem, device })
        | ProviderAction::Btrfs(BtrfsAction::RemoveDevice { filesystem, device }) => {
            vec![*filesystem, *device]
        }
        ProviderAction::Btrfs(BtrfsAction::ConvertProfiles { filesystem, .. })
        | ProviderAction::Btrfs(BtrfsAction::CreateSubvolume { filesystem, .. }) => {
            vec![*filesystem]
        }
        ProviderAction::Mount(MountAction::Mount { source, .. }) => {
            mount_source_node(source).into_iter().collect()
        }
        ProviderAction::Mount(MountAction::Persist { entry }) => {
            mount_source_node(&entry.source).into_iter().collect()
        }
        ProviderAction::Mount(
            MountAction::Unmount { .. } | MountAction::RemovePersistent { .. },
        ) => Vec::new(),
    };
    let mut seen = HashSet::new();
    nodes.retain(|node| seen.insert(*node));
    nodes
}

/// Returns the graph node carried by a typed mount source.
fn mount_source_node(source: &MountSource) -> Option<NodeId> {
    match source {
        MountSource::Filesystem(node)
        | MountSource::BtrfsSubvolume(node)
        | MountSource::ZfsDataset(node) => Some(*node),
        MountSource::Bind(_)
        | MountSource::Tmpfs
        | MountSource::Network(_)
        | MountSource::Other(_) => None,
    }
}

/// Reports whether an action output establishes an external condition.
fn output_satisfies(output: &ActionOutput, prerequisite: &ActionPrerequisite) -> bool {
    match (output, prerequisite) {
        (ActionOutput::Node { id, .. }, ActionPrerequisite::NodeExists(node))
        | (ActionOutput::Node { id, .. }, ActionPrerequisite::NodeKind { node, .. }) => id == node,
        (
            ActionOutput::BlockInterface {
                node,
                access: BlockInterfaceAccess::ReadWrite,
            },
            ActionPrerequisite::Writable(required),
        ) => node == required,
        _ => false,
    }
}

/// Reports conditions intentionally deferred to a live check for baseline nodes.
fn requirement_can_be_checked_live(
    prerequisite: &ActionPrerequisite,
    current: &CurrentState,
) -> bool {
    match prerequisite {
        ActionPrerequisite::Writable(node) => current.graph.node(node).is_some(),
        ActionPrerequisite::DestructiveApproval(_) | ActionPrerequisite::ExtentAvailable { .. } => {
            true
        }
        ActionPrerequisite::NodeExists(_)
        | ActionPrerequisite::NodeKind { .. }
        | ActionPrerequisite::NoUnexpectedConsumers(_)
        | ActionPrerequisite::DistinctNodes(_)
        | ActionPrerequisite::MountTargetAvailable { .. } => false,
    }
}

/// Failure to compose provider fragments into a safe global DAG.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CompositionError {
    /// The number of fragments or actions exceeds the supported identity range.
    #[error("plan contains too many fragments or actions")]
    PlanTooLarge,
    /// A provider returned an error diagnostic with its fragment.
    #[error("fragment {0:?} contains an error diagnostic")]
    FragmentHasErrors(FragmentId),
    /// A fragment reused one local action identity.
    #[error("fragment {fragment:?} contains duplicate local action {action:?}")]
    DuplicateLocalAction {
        /// Fragment containing the duplicate.
        fragment: FragmentId,
        /// Reused local identity.
        action: LocalActionId,
    },
    /// A local dependency references an action absent from its fragment.
    #[error("fragment {0:?} contains an unknown local dependency endpoint")]
    UnknownLocalDependency(FragmentId),
    /// An external requirement references an action absent from its fragment.
    #[error("fragment {fragment:?} requirement references unknown action {action:?}")]
    UnknownRequirementAction {
        /// Fragment containing the invalid requirement.
        fragment: FragmentId,
        /// Unknown local identity.
        action: LocalActionId,
    },
    /// A planned node identity collides with the baseline.
    #[error("planned node {0:?} already exists in the baseline")]
    PlannedNodeAlreadyExists(NodeId),
    /// Two actions claim the same planned node identity.
    #[error("actions {first:?} and {second:?} both create planned node {node:?}")]
    DuplicatePlannedNode {
        /// Conflicting planned identity.
        node: NodeId,
        /// First creating action.
        first: ActionId,
        /// Second creating action.
        second: ActionId,
    },
    /// An action input exists neither in the baseline nor as a planned output.
    #[error("action {action:?} references unresolved node {node:?}")]
    UnresolvedInput {
        /// Action with the unresolved input.
        action: ActionId,
        /// Missing node identity.
        node: NodeId,
    },
    /// A dependency points from an action to itself.
    #[error("action {0:?} depends on itself")]
    SelfDependency(ActionId),
    /// The composed dependency graph contains a cycle.
    #[error("composed action dependencies contain a cycle")]
    DependencyCycle,
    /// An action has a known prerequisite violation in its predecessor state.
    #[error("action {0:?} is rejected in the composed transition order")]
    ActionRejected(ActionId),
    /// A trusted effect failed while analysing one action.
    #[error("action {action:?} effect is invalid: {error}")]
    Effect {
        /// Action whose effect failed.
        action: ActionId,
        /// Effect interpreter failure.
        error: crate::EffectApplicationError,
    },
    /// No baseline evidence or predecessor output establishes a requirement.
    #[error("action {action:?} has an unresolved external requirement")]
    UnresolvedRequirement {
        /// Action owning the unresolved requirement.
        action: ActionId,
    },
    /// More than one action claims to establish the same external condition.
    #[error("action {action:?} has multiple providers for one external requirement")]
    AmbiguousRequirement {
        /// Action owning the ambiguous requirement.
        action: ActionId,
    },
    /// A fragment declared a condition absent from the trusted action contract.
    #[error("action {action:?} declares a requirement absent from its trusted contract")]
    RequirementNotInContract {
        /// Action with a mismatched requirement declaration.
        action: ActionId,
    },
    /// Internal analysis did not produce a prerequisite check.
    #[error("action {0:?} requirement produced no check result")]
    MissingRequirementCheck(ActionId),
    /// Internal analysis did not produce a trusted action contract.
    #[error("action {0:?} has no analysed contract")]
    MissingAnalysedContract(ActionId),
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use storage_core::model::{
        Bytes, Diagnostic, Environment, FilesystemKind, LvmLvKind, MountContext, MountState, Node,
        NodeFacts, NodeKind, NodeSpec, SystemEnvironment,
    };
    use storage_provider::{
        ExternalRequirement, FragmentAction, FragmentDependency, LvmAction, MountAction,
    };

    use super::*;

    /// Builds a baseline containing one existing volume group.
    fn current_with_vg() -> (CurrentState, NodeId) {
        let vg = NodeId::new();
        let mut graph = storage_core::model::NodeGraph::new();
        graph.insert_node(
            vg,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::LvmVg {
                        name: "vg0".to_owned(),
                        extent_size: Some(Bytes::new(4 * 1024 * 1024)),
                    },
                    size: None,
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
            vg,
        )
    }

    /// Creates a fragment origin for one planned node.
    fn origin(subject: NodeId, description: &str) -> FragmentOrigin {
        FragmentOrigin::Change {
            subject,
            description: description.to_owned(),
        }
    }

    /// Composition interleaves provider fragments by their semantic dependencies.
    #[test]
    fn composes_lv_activation_filesystem_and_mount() {
        let (current, vg) = current_with_vg();
        let lv = NodeId::new();
        let filesystem = NodeId::new();
        let create_filesystem = LocalActionId::new(10);
        let mount = LocalActionId::new(20);
        let create_lv = LocalActionId::new(30);
        let activate_lv = LocalActionId::new(31);
        let fragments = vec![
            PlanFragment {
                actions: vec![FragmentAction {
                    id: create_filesystem,
                    operation: ProviderAction::Filesystem(FilesystemAction::Create {
                        backing: lv,
                        planned_node_id: filesystem,
                        filesystem: FilesystemKind::Ext4,
                        label: Some("data".to_owned()),
                    }),
                }],
                dependencies: Vec::new(),
                external_requirements: vec![ExternalRequirement {
                    action: create_filesystem,
                    prerequisite: ActionPrerequisite::Writable(lv),
                }],
                origin: origin(filesystem, "create filesystem"),
                diagnostics: Vec::new(),
            },
            PlanFragment {
                actions: vec![FragmentAction {
                    id: mount,
                    operation: ProviderAction::Mount(MountAction::Mount {
                        source: MountSource::Filesystem(filesystem),
                        target: PathBuf::from("/data"),
                        options: vec!["defaults".to_owned()],
                        context: MountContext::Host,
                    }),
                }],
                dependencies: Vec::new(),
                external_requirements: vec![ExternalRequirement {
                    action: mount,
                    prerequisite: ActionPrerequisite::NodeExists(filesystem),
                }],
                origin: origin(filesystem, "mount filesystem"),
                diagnostics: Vec::new(),
            },
            PlanFragment {
                actions: vec![
                    FragmentAction {
                        id: create_lv,
                        operation: ProviderAction::Lvm(LvmAction::CreateLv {
                            vg,
                            planned_node_id: lv,
                            name: "data".to_owned(),
                            size: Bytes::new(1024 * 1024 * 1024),
                            layout: LvmLvKind::Linear,
                        }),
                    },
                    FragmentAction {
                        id: activate_lv,
                        operation: ProviderAction::Lvm(LvmAction::SetActivation {
                            target: lv,
                            active: true,
                        }),
                    },
                ],
                dependencies: vec![FragmentDependency {
                    before: create_lv,
                    after: activate_lv,
                    reason: DependencyReason::InterfacePreparation,
                }],
                external_requirements: Vec::new(),
                origin: origin(lv, "create and activate LV"),
                diagnostics: Vec::new(),
            },
        ];

        let plan = compose_plan(&current, fragments)
            .unwrap_or_else(|error| panic!("compose cross-provider plan: {error}"));
        let ordered = plan.ordered_actions();

        assert!(matches!(
            ordered[0].operation,
            ProviderAction::Lvm(LvmAction::CreateLv { .. })
        ));
        assert!(matches!(
            ordered[1].operation,
            ProviderAction::Lvm(LvmAction::SetActivation { .. })
        ));
        assert!(matches!(
            ordered[2].operation,
            ProviderAction::Filesystem(FilesystemAction::Create { .. })
        ));
        assert!(matches!(
            ordered[3].operation,
            ProviderAction::Mount(MountAction::Mount { .. })
        ));
        assert!(plan.dependencies().iter().any(|dependency| {
            dependency.before == ordered[1].id
                && dependency.after == ordered[2].id
                && dependency.reason == DependencyReason::InterfacePreparation
        }));
    }

    /// Two actions cannot claim the same planned identity.
    #[test]
    fn rejects_duplicate_planned_node() {
        let (current, vg) = current_with_vg();
        let lv = NodeId::new();
        let fragment = PlanFragment {
            actions: vec![
                FragmentAction {
                    id: LocalActionId::new(1),
                    operation: ProviderAction::Lvm(LvmAction::CreateLv {
                        vg,
                        planned_node_id: lv,
                        name: "one".to_owned(),
                        size: Bytes::new(1024),
                        layout: LvmLvKind::Linear,
                    }),
                },
                FragmentAction {
                    id: LocalActionId::new(2),
                    operation: ProviderAction::Lvm(LvmAction::CreateLv {
                        vg,
                        planned_node_id: lv,
                        name: "two".to_owned(),
                        size: Bytes::new(1024),
                        layout: LvmLvKind::Linear,
                    }),
                },
            ],
            dependencies: Vec::new(),
            external_requirements: Vec::new(),
            origin: origin(lv, "duplicate LV"),
            diagnostics: Vec::new(),
        };

        assert!(matches!(
            compose_plan(&current, vec![fragment]),
            Err(CompositionError::DuplicatePlannedNode { node, .. }) if node == lv
        ));
    }

    /// Local dependency cycles are rejected before any transition is accepted.
    #[test]
    fn rejects_dependency_cycle() {
        let (current, vg) = current_with_vg();
        let first = LocalActionId::new(1);
        let second = LocalActionId::new(2);
        let fragment = PlanFragment {
            actions: vec![
                FragmentAction {
                    id: first,
                    operation: ProviderAction::Lvm(LvmAction::SetActivation {
                        target: vg,
                        active: true,
                    }),
                },
                FragmentAction {
                    id: second,
                    operation: ProviderAction::Lvm(LvmAction::SetActivation {
                        target: vg,
                        active: false,
                    }),
                },
            ],
            dependencies: vec![
                FragmentDependency {
                    before: first,
                    after: second,
                    reason: DependencyReason::RequestedOrder,
                },
                FragmentDependency {
                    before: second,
                    after: first,
                    reason: DependencyReason::RequestedOrder,
                },
            ],
            external_requirements: Vec::new(),
            origin: origin(vg, "activation cycle"),
            diagnostics: Vec::new(),
        };

        assert_eq!(
            compose_plan(&current, vec![fragment]),
            Err(CompositionError::DependencyCycle)
        );
    }
}
