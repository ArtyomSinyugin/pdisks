//! Composition of provider fragments into one validated action DAG.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use storage_core::model::{
    CurrentState, DesiredState, Diagnostic, DiagnosticSeverity, MountContext, MountSource, NodeId,
    OperationIntent,
};
use storage_provider::{
    ActionContract, ActionDescriptionContext, ActionOutput, ActionPrerequisite, ActionResource,
    BlockInterfaceAccess, BtrfsAction, DependencyReason, FilesystemAction, FragmentOrigin,
    LocalActionId, LuksAction, LvmAction, MountAction, PartitionAction, PlanFragment,
    ProviderAction, ResourceAccessMode, describe_action,
};
use thiserror::Error;

use crate::{
    ActionDependency, ActionId, ActionSimulationError, CapacityEvidence, CapacityEvidenceError,
    ExecutionPlan, FragmentActionRef, FragmentId, FragmentOrdering, GoalAspect, GoalStatus,
    PlanSimulationError, PlannedAction, PrerequisiteCheck, ProtectedAspect, ProtectedScope,
    simulate_plan_for_goal_with_policy,
};

use crate::{
    effects::simulate_action, prerequisites::evaluate_prerequisites,
    simulation_evidence::SimulationEvidence,
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

/// Trusted contracts and their checks in one candidate predecessor trace.
struct AnalysedOrder {
    /// Recalculated contracts indexed by global action identity.
    contracts: HashMap<ActionId, ActionContract>,
    /// Conditions evaluated immediately before each action.
    checks: HashMap<ActionId, Vec<PrerequisiteCheck>>,
}

/// Builds one immutable plan from provider-owned fragments.
pub fn compose_plan(
    current: &CurrentState,
    fragments: Vec<PlanFragment>,
) -> Result<ExecutionPlan, CompositionError> {
    compose_plan_with_evidence(current, fragments, &[])
}

/// Builds a plan and proves coverage of every explicit goal intent.
/// Unknown evidence remains conditional; inspect it with `simulate_plan_for_goal`.
pub fn compose_plan_for_goal(
    current: &CurrentState,
    goal: &DesiredState,
    fragments: Vec<PlanFragment>,
) -> Result<ExecutionPlan, CompositionError> {
    compose_plan_for_goal_with_evidence(current, goal, fragments, &[])
}

/// Builds a goal-covering plan using provider capacity evidence for the baseline.
/// Unknown evidence remains conditional; inspect it with the goal-aware simulator.
pub fn compose_plan_for_goal_with_evidence(
    current: &CurrentState,
    goal: &DesiredState,
    fragments: Vec<PlanFragment>,
    capacity_evidence: &[CapacityEvidence],
) -> Result<ExecutionPlan, CompositionError> {
    compose_plan_for_goal_with_policy(
        current,
        goal,
        fragments,
        capacity_evidence,
        &ProtectedScope::default(),
    )
}

/// Builds a goal-covering plan while rejecting changes to explicitly protected objects.
pub fn compose_plan_for_goal_with_policy(
    current: &CurrentState,
    goal: &DesiredState,
    fragments: Vec<PlanFragment>,
    capacity_evidence: &[CapacityEvidence],
    protected: &ProtectedScope,
) -> Result<ExecutionPlan, CompositionError> {
    validate_intent_coverage(&goal.intents, &fragments)?;
    let plan = compose_plan_with_evidence(current, fragments, capacity_evidence)?;
    let report =
        simulate_plan_for_goal_with_policy(current, &plan, goal, capacity_evidence, protected)
            .map_err(CompositionError::FinalSimulation)?;
    if let Some(violation) = report.protection_violations.into_iter().next() {
        return Err(CompositionError::ProtectedScopeViolation {
            action: violation.action,
            aspect: violation.aspect,
        });
    }
    if let Some(check) = report
        .goal_checks
        .into_iter()
        .find(|check| matches!(check.status, GoalStatus::Violated { .. }))
    {
        return Err(CompositionError::GoalNotReached(check.aspect));
    }
    Ok(plan)
}

/// Builds one immutable plan using provider capacity evidence for the baseline.
pub fn compose_plan_with_evidence(
    current: &CurrentState,
    fragments: Vec<PlanFragment>,
    capacity_evidence: &[CapacityEvidence],
) -> Result<ExecutionPlan, CompositionError> {
    compose_plan_with_ordering(current, fragments, &[], capacity_evidence)
}

/// Builds a plan with explicit cross-fragment edges before any transition is simulated.
/// Endpoints belong to this input set; mandatory inferred dependencies still apply.
pub fn compose_plan_with_ordering(
    current: &CurrentState,
    fragments: Vec<PlanFragment>,
    ordering: &[FragmentOrdering],
    capacity_evidence: &[CapacityEvidence],
) -> Result<ExecutionPlan, CompositionError> {
    current.graph.validate_structure()?;
    let FlattenedFragments {
        actions: draft,
        mut dependencies,
        requirements,
    } = flatten_fragments(&fragments)?;
    let endpoints: HashMap<_, _> = draft
        .iter()
        .map(|action| {
            (
                FragmentActionRef {
                    fragment: action.fragment,
                    action: action.local_id,
                },
                action.id,
            )
        })
        .collect();
    for edge in ordering {
        let resolve = |endpoint: FragmentActionRef| {
            endpoints
                .get(&endpoint)
                .copied()
                .ok_or(CompositionError::UnknownOrderingAction(endpoint))
        };
        add_dependency(
            &mut dependencies,
            resolve(edge.before)?,
            resolve(edge.after)?,
            edge.reason,
        )?;
    }
    bind_planned_nodes(current, &draft, &mut dependencies)?;
    bind_mount_order(&draft, &mut dependencies)?;
    let first_contracts = simulate_order(
        current,
        capacity_evidence,
        &draft,
        &requirements,
        &mut dependencies,
    )?;
    resolve_requirements(&requirements, &first_contracts, &mut dependencies)?;

    let final_contracts =
        simulate_order(current, capacity_evidence, &draft, &[], &mut dependencies)?.contracts;
    validate_declared_requirements(&requirements, &final_contracts)?;
    validate_resource_order(&final_contracts, &dependencies)?;

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

/// Ensures each explicit intent owns actions that implement its typed operation.
fn validate_intent_coverage(
    intents: &[OperationIntent],
    fragments: &[PlanFragment],
) -> Result<(), CompositionError> {
    for (fragment_index, fragment) in fragments.iter().enumerate() {
        if let FragmentOrigin::Intent { index } = fragment.origin
            && index >= intents.len()
        {
            return Err(CompositionError::UnknownIntentOrigin {
                fragment: FragmentId::new(
                    u32::try_from(fragment_index).map_err(|_| CompositionError::PlanTooLarge)?,
                ),
                index,
            });
        }
    }
    for (index, intent) in intents.iter().enumerate() {
        if !intent_is_covered(index, intent, fragments) {
            return Err(CompositionError::UncoveredIntent { index });
        }
    }
    Ok(())
}

/// Reports whether matching-origin actions completely implement one supported intent.
fn intent_is_covered(index: usize, intent: &OperationIntent, fragments: &[PlanFragment]) -> bool {
    let actions = fragments
        .iter()
        .filter(|fragment| fragment.origin == FragmentOrigin::Intent { index })
        .flat_map(|fragment| fragment.actions.iter().map(|action| &action.operation))
        .collect::<Vec<_>>();
    actions_cover_intent(intent, &actions)
}

/// Shares typed intent matching between composition and simulation reports.
pub(crate) fn actions_cover_intent(intent: &OperationIntent, actions: &[&ProviderAction]) -> bool {
    match intent {
        OperationIntent::Check { target, force } => actions.iter().any(|action| {
            matches!(
                action,
                ProviderAction::Filesystem(FilesystemAction::Check { filesystem, force: action_force })
                    if filesystem == target && action_force == force
            )
        }),
        OperationIntent::Repair { target, force } => actions.iter().any(|action| {
            matches!(
                action,
                ProviderAction::Filesystem(FilesystemAction::Repair { filesystem, force: action_force })
                    if filesystem == target && action_force == force
            )
        }),
        OperationIntent::ConvertBtrfsProfiles { filesystem, target } => {
            actions.iter().any(|action| {
                matches!(
                    action,
                    ProviderAction::Btrfs(BtrfsAction::ConvertProfiles {
                        filesystem: action_filesystem,
                        target: action_target,
                    }) if action_filesystem == filesystem && action_target == target
                )
            })
        }
        OperationIntent::SetActivation { target, active } => actions.iter().any(|action| {
            matches!(
                action,
                ProviderAction::Lvm(LvmAction::SetActivation { target: action_target, active: action_active })
                    if action_target == target && action_active == active
            )
        }),
        OperationIntent::ReplaceMember { group, old, new } => {
            let removes_old = actions.iter().any(|action| {
                matches!(
                    action,
                    ProviderAction::Btrfs(BtrfsAction::RemoveDevice { filesystem, device })
                        if filesystem == group && device == old
                )
            });
            let adds_new = actions.iter().any(|action| {
                matches!(
                    action,
                    ProviderAction::Btrfs(BtrfsAction::AddDevice { filesystem, device })
                        if filesystem == group && device == new
                )
            });
            removes_old && adds_new
        }
        // shortcut: reformat needs explicit replacement semantics before Create can prove coverage.
        OperationIntent::Reformat { .. } => false,
    }
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

/// Adds child-removal and parent-preparation edges before the first simulation trace.
fn bind_mount_order(
    actions: &[DraftAction],
    dependencies: &mut Vec<ActionDependency>,
) -> Result<(), CompositionError> {
    for parent in actions {
        let Some((parent_target, parent_context)) = runtime_mount_target(&parent.operation) else {
            continue;
        };
        for child in actions {
            let Some((child_target, child_context)) = runtime_mount_target(&child.operation) else {
                continue;
            };
            if parent_context != child_context
                || parent_target == child_target
                || !child_target.starts_with(parent_target)
            {
                continue;
            }
            // Explicit recipes can contain repeated mount/unmount phases; replay validates them.
            if dependency_path(parent.id, child.id, dependencies)
                || dependency_path(child.id, parent.id, dependencies)
            {
                continue;
            }
            let (before, after) = if matches!(
                child.operation,
                ProviderAction::Mount(MountAction::Unmount { .. })
            ) {
                (child.id, parent.id)
            } else {
                (parent.id, child.id)
            };
            add_dependency(dependencies, before, after, DependencyReason::MountOrder)?;
        }
    }
    Ok(())
}

/// Returns namespace-qualified targets only for runtime mount changes.
fn runtime_mount_target(action: &ProviderAction) -> Option<(&Path, &MountContext)> {
    match action {
        ProviderAction::Mount(
            MountAction::Mount {
                target, context, ..
            }
            | MountAction::Unmount { target, context },
        ) => Some((target, context)),
        _ => None,
    }
}

/// Binds a missing declared output in the consumer's current predecessor context.
/// Already replayed actions cannot supply a revoked guarantee by being retried implicitly.
fn bind_contextual_outputs(
    desired: &DesiredState,
    evidence: &SimulationEvidence,
    action: &DraftAction,
    actions: &[DraftAction],
    emitted: &HashMap<ActionId, ActionContract>,
    requirements: &[PendingRequirement],
    dependencies: &mut Vec<ActionDependency>,
) -> Result<bool, CompositionError> {
    let owned = requirements
        .iter()
        .filter(|requirement| requirement.action == action.id)
        .collect::<Vec<_>>();
    if owned.is_empty() {
        return Ok(false);
    }
    let context = ActionDescriptionContext {
        graph: &desired.graph,
    };
    let contract = describe_action(&action.operation, context);
    let providers = actions
        .iter()
        .filter(|candidate| !emitted.contains_key(&candidate.id))
        .map(|candidate| (candidate.id, describe_action(&candidate.operation, context)))
        .collect();
    for requirement in owned {
        if !contract.prerequisites.contains(&requirement.prerequisite) {
            return Err(CompositionError::RequirementNotInContract { action: action.id });
        }
        let check = evaluate_prerequisites(
            desired,
            evidence,
            std::slice::from_ref(&requirement.prerequisite),
        );
        if matches!(check[0].status, crate::CheckStatus::Satisfied(_)) {
            continue;
        }
        if let Some(provider) = requirement_provider(requirement, &providers, dependencies)? {
            // An unexecuted provider cannot already precede this action in the current topological order.
            add_dependency(
                dependencies,
                provider,
                action.id,
                DependencyReason::InterfacePreparation,
            )?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Selects one output provider, honoring explicit phases and rejecting unordered alternatives.
fn requirement_provider(
    requirement: &PendingRequirement,
    contracts: &HashMap<ActionId, ActionContract>,
    dependencies: &[ActionDependency],
) -> Result<Option<ActionId>, CompositionError> {
    let providers = contracts
        .iter()
        .filter_map(|(action, contract)| {
            (*action != requirement.action
                && !dependency_path(requirement.action, *action, dependencies)
                && contract
                    .provided_outputs
                    .iter()
                    .any(|output| output_satisfies(output, &requirement.prerequisite)))
            .then_some(*action)
        })
        .collect::<Vec<_>>();
    // A unique latest explicitly ordered predecessor supplies a multi-phase recipe.
    let predecessors = providers
        .iter()
        .copied()
        .filter(|provider| dependency_path(*provider, requirement.action, dependencies))
        .collect::<Vec<_>>();
    if let Some(latest) = predecessors.iter().copied().find(|candidate| {
        predecessors
            .iter()
            .all(|other| other == candidate || dependency_path(*other, *candidate, dependencies))
    }) {
        return Ok(Some(latest));
    }
    match providers.as_slice() {
        [] => Ok(None),
        [provider] => Ok(Some(*provider)),
        _ => Err(CompositionError::AmbiguousRequirement {
            action: requirement.action,
        }),
    }
}

/// Resolves requirements from the consumer's predecessor checks, never its final state.
fn resolve_requirements(
    requirements: &[PendingRequirement],
    trace: &AnalysedOrder,
    dependencies: &mut Vec<ActionDependency>,
) -> Result<(), CompositionError> {
    validate_declared_requirements(requirements, &trace.contracts)?;
    for requirement in requirements {
        let check = trace
            .checks
            .get(&requirement.action)
            .and_then(|checks| {
                checks
                    .iter()
                    .find(|check| check.prerequisite == requirement.prerequisite)
            })
            .ok_or(CompositionError::MissingRequirementCheck(
                requirement.action,
            ))?;
        if matches!(
            check.status,
            crate::CheckStatus::Satisfied(crate::CheckEvidence::ObservedSnapshot)
        ) {
            continue;
        }
        if let Some(provider) = requirement_provider(requirement, &trace.contracts, dependencies)? {
            add_dependency(
                dependencies,
                provider,
                requirement.action,
                DependencyReason::InterfacePreparation,
            )?;
        } else if matches!(check.status, crate::CheckStatus::Violated { .. }) {
            return Err(CompositionError::UnresolvedRequirement {
                action: requirement.action,
            });
        }
        // Satisfied effects without named outputs still require resource ordering below.
        // Unknown trusted conditions remain explicit live obligations in the simulation report.
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

/// Rejects conflicting accesses when the DAG does not define their order.
pub(crate) fn validate_resource_order(
    contracts: &HashMap<ActionId, ActionContract>,
    dependencies: &[ActionDependency],
) -> Result<(), CompositionError> {
    let mut actions = contracts.keys().copied().collect::<Vec<_>>();
    actions.sort_unstable();
    for (index, first) in actions.iter().enumerate() {
        for second in &actions[index + 1..] {
            let first_contract = &contracts[first];
            let second_contract = &contracts[second];
            let Some(resource) = conflicting_resource(first_contract, second_contract) else {
                continue;
            };
            if !dependency_path(*first, *second, dependencies)
                && !dependency_path(*second, *first, dependencies)
            {
                return Err(CompositionError::AmbiguousResourceOrder {
                    first: *first,
                    second: *second,
                    resource,
                });
            }
        }
    }
    Ok(())
}

/// Returns one shared resource for which at least one action requests a write.
fn conflicting_resource(first: &ActionContract, second: &ActionContract) -> Option<ActionResource> {
    first.affected_resources.iter().find_map(|left| {
        second.affected_resources.iter().find_map(|right| {
            (left.resource == right.resource
                && (left.mode == ResourceAccessMode::Write
                    || right.mode == ResourceAccessMode::Write))
                .then(|| left.resource.clone())
        })
    })
}

/// Reports whether the dependency DAG orders one action before another.
fn dependency_path(before: ActionId, after: ActionId, dependencies: &[ActionDependency]) -> bool {
    let mut pending = vec![before];
    let mut visited = HashSet::new();
    while let Some(current) = pending.pop() {
        if !visited.insert(current) {
            continue;
        }
        for dependency in dependencies
            .iter()
            .filter(|dependency| dependency.before == current)
        {
            if dependency.after == after {
                return true;
            }
            pending.push(dependency.after);
        }
    }
    false
}

/// Replays a candidate order, restarting from C0 whenever a contextual output adds an edge.
/// Every restart adds a new dependency; final replay passes no requirements and cannot alter the DAG.
fn simulate_order(
    current: &CurrentState,
    capacity_evidence: &[CapacityEvidence],
    actions: &[DraftAction],
    requirements: &[PendingRequirement],
    dependencies: &mut Vec<ActionDependency>,
) -> Result<AnalysedOrder, CompositionError> {
    loop {
        let order = topological_order(actions.len(), dependencies)?;
        let mut desired = DesiredState::from_current(current);
        let mut evidence = SimulationEvidence::new(capacity_evidence)
            .map_err(CompositionError::CapacityEvidence)?;
        let mut contracts = HashMap::new();
        let mut checks = HashMap::new();
        let mut rebound = false;
        for id in order {
            let action = &actions[id.get() as usize];
            if bind_contextual_outputs(
                &desired,
                &evidence,
                action,
                actions,
                &contracts,
                requirements,
                dependencies,
            )? {
                rebound = true;
                break;
            }
            match simulate_action(&mut desired, &mut evidence, &action.operation) {
                Ok(result) => {
                    contracts.insert(id, result.contract);
                    checks.insert(id, result.checks);
                }
                Err(ActionSimulationError::Validation { diagnostics }) => {
                    return Err(CompositionError::ActionValidation {
                        action: id,
                        diagnostics,
                    });
                }
                Err(ActionSimulationError::PrerequisitesViolated { checks }) => {
                    return Err(CompositionError::ActionRejected { action: id, checks });
                }
                Err(ActionSimulationError::Effect(error)) => {
                    return Err(CompositionError::Effect { action: id, error });
                }
            }
        }
        if !rebound {
            return Ok(AnalysedOrder { contracts, checks });
        }
    }
}

/// Calculates a stable topological order or reports a cycle.
pub(crate) fn topological_order(
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
        (ActionOutput::Node { id, .. }, ActionPrerequisite::NodeExists(node)) => id == node,
        (ActionOutput::Node { id, spec }, ActionPrerequisite::NodeKind { node, expected }) => {
            id == node && crate::prerequisites::matches_kind(&spec.kind, *expected)
        }
        (
            ActionOutput::FilesystemCheckPassed(filesystem),
            ActionPrerequisite::FilesystemCheckPassed(required),
        ) => filesystem == required,
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

/// Failure to compose provider fragments into a safe global DAG.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CompositionError {
    /// Explicit cross-fragment ordering refers to an absent fragment or local action.
    #[error("ordering references absent action {0:?}")]
    UnknownOrderingAction(FragmentActionRef),
    /// The supplied storage graph has an invalid representation.
    #[error(transparent)]
    Structure(#[from] storage_core::model::GraphError),
    /// A planned mutation intersects the caller's explicit protected area.
    #[error("action {action:?} modifies protected aspect {aspect:?}")]
    ProtectedScopeViolation {
        /// Action attempting the protected change.
        action: ActionId,
        /// Protected node or mount target affected by that action.
        aspect: ProtectedAspect,
    },
    /// A provider rejected action parameters or its intermediate graph context.
    #[error("action {action:?} failed technology-specific validation")]
    ActionValidation {
        /// Global identity of the rejected action.
        action: ActionId,
        /// Provider diagnostics retained for review and caller reporting.
        diagnostics: Vec<Diagnostic>,
    },
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
    #[error("action {action:?} is rejected in the composed transition order")]
    ActionRejected {
        /// Action rejected in its predecessor state.
        action: ActionId,
        /// Complete prerequisite results, including violated, satisfied, and unknown checks.
        checks: Vec<PrerequisiteCheck>,
    },
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
    /// Provider evidence contains duplicate allocation domains.
    #[error(transparent)]
    CapacityEvidence(#[from] CapacityEvidenceError),
    /// Two actions conflict on one resource without a semantic ordering edge.
    #[error(
        "actions {first:?} and {second:?} access resource {resource:?} without an ordering dependency"
    )]
    AmbiguousResourceOrder {
        /// First conflicting action.
        first: ActionId,
        /// Second conflicting action.
        second: ActionId,
        /// Shared resource that requires serialization.
        resource: ActionResource,
    },
    /// A fragment claims an intent index absent from the requested goal.
    #[error("fragment {fragment:?} references unknown intent {index}")]
    UnknownIntentOrigin {
        /// Fragment containing the invalid origin.
        fragment: FragmentId,
        /// Out-of-range intent index.
        index: usize,
    },
    /// No matching typed actions cover one explicit requested intent.
    #[error("requested intent {index} is not covered by plan actions")]
    UncoveredIntent {
        /// Index in `DesiredState::intents`.
        index: usize,
    },
    /// Replaying the frozen plan failed during final goal analysis.
    #[error("final plan simulation failed: {0}")]
    FinalSimulation(PlanSimulationError),
    /// The final projection has a known mismatch with the requested goal.
    #[error("final plan projection does not reach goal aspect {0:?}")]
    GoalNotReached(GoalAspect),
}

#[cfg(test)]
mod tests {
    use crate::simulation_evidence::TestSimulation;
    use std::path::PathBuf;

    use storage_core::model::{
        BootSpec, Bytes, Dependency, DependencyKind, Diagnostic, Environment, FilesystemKind,
        LvmLv, LvmLvKind, MountContext, MountSpec, MountState, Node, NodeFacts, NodeKind, NodeSpec,
        SystemEnvironment,
    };
    use storage_provider::{
        AllocationDomain, AllocationQuantity, ExternalRequirement, FragmentAction,
        FragmentDependency, LvmAction, MountAction,
    };

    use super::*;

    /// Technology validation sees predecessor outputs and preserves rejection diagnostics.
    #[test]
    fn validates_luks_resize_against_the_intermediate_graph() {
        let (mut current, _) = current_with_vg();
        let disk = NodeId::new();
        current
            .graph
            .insert_new(
                disk,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: Some(Bytes::new(1024)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let baseline = current.clone();
        let container = NodeId::new();
        let mapping = NodeId::new();
        let credential = storage_provider::CredentialRef::from_uuid(disk.as_uuid());
        for size in [0, 16] {
            let operations = vec![
                LuksAction::Format {
                    target: disk,
                    planned_node_id: container,
                    version: storage_core::model::LuksVersion::Luks2,
                    credential,
                },
                LuksAction::Open {
                    container,
                    planned_node_id: mapping,
                    mapping_name: "data".to_owned(),
                    credential,
                    read_only: false,
                },
                LuksAction::Resize {
                    mapping,
                    new_size: Some(Bytes::new(size)),
                },
            ];
            let fragment = PlanFragment {
                actions: operations
                    .into_iter()
                    .zip(0_u32..)
                    .map(|(operation, id)| FragmentAction {
                        id: LocalActionId::new(id),
                        operation: ProviderAction::Luks(operation),
                    })
                    .collect(),
                dependencies: Vec::new(),
                external_requirements: Vec::new(),
                origin: origin(mapping, "format, open, and resize LUKS"),
                diagnostics: Vec::new(),
            };
            let result = compose_plan(&current, vec![fragment]);
            if size == 0 {
                assert!(matches!(
                    result,
                    Err(CompositionError::ActionValidation { action, diagnostics })
                        if action == ActionId::new(2)
                            && diagnostics.iter().any(|diagnostic|
                                diagnostic.code == "luks.resize.zero_size")
                ));
            } else {
                let plan = result.unwrap_or_else(|error| panic!("compose LUKS plan: {error}"));
                let simulation = crate::simulate_plan(&current, &plan)
                    .unwrap_or_else(|error| panic!("replay LUKS plan: {error}"));
                assert_eq!(simulation.steps.len(), 3);
            }
            assert_eq!(current, baseline);
        }
    }

    /// Step-local invalidations identify their action while the result retains cumulative evidence.
    #[test]
    fn simulation_trace_retains_observation_invalidations() {
        let (current, subject) = current_with_vg();
        let mut fragment = mount_fragment(
            subject,
            vec![
                MountAction::Mount {
                    source: MountSource::Tmpfs,
                    target: PathBuf::from("/data"),
                    options: Vec::new(),
                    context: MountContext::Host,
                },
                MountAction::Unmount {
                    target: PathBuf::from("/data"),
                    context: MountContext::Host,
                },
            ],
        );
        fragment.dependencies.push(FragmentDependency {
            before: LocalActionId::new(0),
            after: LocalActionId::new(1),
            reason: DependencyReason::RequestedOrder,
        });
        let plan = compose_plan(&current, vec![fragment])
            .unwrap_or_else(|error| panic!("compose invalidation trace: {error}"));
        let result = crate::simulate_plan(&current, &plan)
            .unwrap_or_else(|error| panic!("replay invalidation trace: {error}"));
        let invalidation = storage_provider::FactInvalidation {
            scope: storage_provider::FactScope::MountTarget {
                target: PathBuf::from("/data"),
                context: MountContext::Host,
            },
            kind: storage_provider::FactKind::Mount,
        };
        assert_eq!(result.steps.len(), 2);
        for step in &result.steps {
            assert_eq!(step.invalidated_facts, vec![invalidation.clone()]);
        }
        assert!(
            result
                .evidence
                .is_observation_stale(&invalidation.scope, invalidation.kind)
        );
        assert!(
            !result
                .evidence
                .is_observation_stale(&invalidation.scope, storage_provider::FactKind::Geometry,)
        );
        assert_eq!(
            result.evidence.invalidated_facts.as_slice(),
            &[invalidation]
        );
        assert_eq!(current.mounts.entries.len(), 0);
    }

    /// Persistent configuration cannot refer to missing objects or objects of the wrong kind.
    #[test]
    fn persistent_mount_requires_an_existing_compatible_source() {
        let (current, lv, filesystem) = current_with_lv_and_filesystem();
        for source in [
            MountSource::Filesystem(lv),
            MountSource::Filesystem(NodeId::new()),
            MountSource::BtrfsSubvolume(filesystem),
            MountSource::ZfsDataset(filesystem),
        ] {
            let fragment = mount_fragment(
                lv,
                vec![MountAction::Persist {
                    entry: storage_core::model::MountEntry {
                        source,
                        target: PathBuf::from("/data"),
                        options: Vec::new(),
                        context: MountContext::Host,
                    },
                }],
            );
            assert!(compose_plan(&current, vec![fragment]).is_err());
        }
        let fragment = mount_fragment(
            lv,
            vec![MountAction::Persist {
                entry: storage_core::model::MountEntry {
                    source: MountSource::Filesystem(filesystem),
                    target: PathBuf::from("/data"),
                    options: Vec::new(),
                    context: MountContext::Host,
                },
            }],
        );
        let plan = compose_plan(&current, vec![fragment]).unwrap_or_else(|error| {
            panic!("persist compatible source without activation: {error}")
        });
        assert!(
            plan.actions()[0]
                .informational_contract
                .prerequisites
                .contains(&ActionPrerequisite::NodeExists(filesystem))
        );
        assert!(
            plan.actions()[0]
                .informational_contract
                .affected_resources
                .iter()
                .any(|access| access.resource
                    == storage_provider::ActionResource::Node(filesystem)
                    && access.mode == storage_provider::ResourceAccessMode::Read)
        );
    }

    /// Wraps runtime actions with stable local identities for mount-order scenarios.
    fn mount_fragment(subject: NodeId, operations: Vec<MountAction>) -> PlanFragment {
        PlanFragment {
            actions: operations
                .into_iter()
                .zip(0_u32..)
                .map(|(operation, id)| FragmentAction {
                    id: LocalActionId::new(id),
                    operation: ProviderAction::Mount(operation),
                })
                .collect(),
            dependencies: Vec::new(),
            external_requirements: Vec::new(),
            origin: origin(subject, "runtime mount changes"),
            diagnostics: Vec::new(),
        }
    }

    /// Nested actions are ordered before simulation, including requirements satisfied by removal.
    #[test]
    fn composes_nested_mounts_and_unmounts_in_semantic_order() {
        for unmount in [false, true] {
            let (mut current, subject) = current_with_vg();
            if unmount {
                current.mounts.entries = ["/data", "/data/child"]
                    .into_iter()
                    .map(|target| storage_core::model::ObservedMount {
                        source: MountSource::Tmpfs,
                        target: PathBuf::from(target),
                        options: Vec::new(),
                        context: MountContext::Host,
                    })
                    .collect();
            }
            let baseline = current.clone();
            let targets = if unmount {
                ["/data", "/data/child"]
            } else {
                ["/data/child", "/data"]
            };
            let mut fragment = mount_fragment(
                subject,
                targets
                    .into_iter()
                    .map(|target| {
                        if unmount {
                            MountAction::Unmount {
                                target: PathBuf::from(target),
                                context: MountContext::Host,
                            }
                        } else {
                            MountAction::Mount {
                                source: MountSource::Tmpfs,
                                target: PathBuf::from(target),
                                options: Vec::new(),
                                context: MountContext::Host,
                            }
                        }
                    })
                    .collect(),
            );
            fragment.external_requirements.push(ExternalRequirement {
                action: LocalActionId::new(if unmount { 0 } else { 1 }),
                prerequisite: ActionPrerequisite::MountDescendantsAbsent {
                    target: PathBuf::from("/data"),
                    context: MountContext::Host,
                },
            });
            let plan = compose_plan(&current, vec![fragment.clone()])
                .unwrap_or_else(|error| panic!("compose nested mount changes: {error}"));
            let ordered = plan
                .ordered_actions()
                .iter()
                .map(|action| action.local_id.get())
                .collect::<Vec<_>>();
            assert_eq!(ordered, vec![1, 0]);
            assert!(
                plan.dependencies()
                    .iter()
                    .any(|dependency| dependency.reason == DependencyReason::MountOrder)
            );
            let simulation = crate::simulate_plan(&current, &plan)
                .unwrap_or_else(|error| panic!("replay nested mount changes: {error}"));
            assert_eq!(
                simulation.desired.mounts.runtime.as_slice().len(),
                if unmount { 0 } else { 2 }
            );
            fragment.dependencies.push(FragmentDependency {
                before: LocalActionId::new(0),
                after: LocalActionId::new(1),
                reason: DependencyReason::RequestedOrder,
            });
            assert!(matches!(
                compose_plan(&current, vec![fragment]),
                Err(CompositionError::ActionRejected { .. })
            ));
            assert_eq!(current, baseline);
        }
    }

    /// Mixed mount/unmount changes prepare the parent before introducing its new descendants.
    #[test]
    fn orders_mixed_parent_and_child_mount_changes() {
        for mount_parent in [false, true] {
            let (mut current, subject) = current_with_vg();
            current
                .mounts
                .entries
                .push(storage_core::model::ObservedMount {
                    source: MountSource::Tmpfs,
                    target: PathBuf::from(if mount_parent { "/data/child" } else { "/data" }),
                    options: Vec::new(),
                    context: MountContext::Host,
                });
            let parent = if mount_parent {
                MountAction::Mount {
                    source: MountSource::Tmpfs,
                    target: PathBuf::from("/data"),
                    options: Vec::new(),
                    context: MountContext::Host,
                }
            } else {
                MountAction::Unmount {
                    target: PathBuf::from("/data"),
                    context: MountContext::Host,
                }
            };
            let child = if mount_parent {
                MountAction::Unmount {
                    target: PathBuf::from("/data/child"),
                    context: MountContext::Host,
                }
            } else {
                MountAction::Mount {
                    source: MountSource::Tmpfs,
                    target: PathBuf::from("/data/child"),
                    options: Vec::new(),
                    context: MountContext::Host,
                }
            };
            let plan = compose_plan(&current, vec![mount_fragment(subject, vec![parent, child])])
                .unwrap_or_else(|error| panic!("compose mixed mount changes: {error}"));
            let ordered = plan
                .ordered_actions()
                .iter()
                .map(|action| action.local_id.get())
                .collect::<Vec<_>>();
            assert_eq!(ordered, if mount_parent { vec![1, 0] } else { vec![0, 1] });
            let simulation = crate::simulate_plan(&current, &plan)
                .unwrap_or_else(|error| panic!("replay mixed mount changes: {error}"));
            assert_eq!(simulation.desired.mounts.runtime.as_slice().len(), 1);
        }
    }

    /// Explicit multi-phase recipes retain their valid order rather than acquiring false cycles.
    #[test]
    fn explicit_nested_mount_cycles_remain_valid() {
        let (current, subject) = current_with_vg();
        let mut operations = vec![
            MountAction::Mount {
                source: MountSource::Tmpfs,
                target: PathBuf::from("/data"),
                options: Vec::new(),
                context: MountContext::Host,
            },
            MountAction::Mount {
                source: MountSource::Tmpfs,
                target: PathBuf::from("/data/child"),
                options: Vec::new(),
                context: MountContext::Host,
            },
            MountAction::Unmount {
                target: PathBuf::from("/data/child"),
                context: MountContext::Host,
            },
            MountAction::Unmount {
                target: PathBuf::from("/data"),
                context: MountContext::Host,
            },
        ];
        operations.extend(operations[..2].to_vec());
        let mut fragment = mount_fragment(subject, operations);
        fragment.dependencies = (0..5)
            .map(|before| FragmentDependency {
                before: LocalActionId::new(before),
                after: LocalActionId::new(before + 1),
                reason: DependencyReason::RequestedOrder,
            })
            .collect();
        let plan = compose_plan(&current, vec![fragment])
            .unwrap_or_else(|error| panic!("compose repeated mount phases: {error}"));
        let simulation = crate::simulate_plan(&current, &plan)
            .unwrap_or_else(|error| panic!("replay repeated mount phases: {error}"));
        assert_eq!(simulation.steps.len(), 6);
        assert_eq!(simulation.desired.mounts.runtime.as_slice().len(), 2);
    }

    /// Lexical siblings and different namespaces do not acquire false parent/child edges.
    #[test]
    fn mount_order_respects_path_components_and_contexts() {
        let (current, subject) = current_with_vg();
        for (child_path, parent_context, child_context) in [
            ("/data2/child", MountContext::Host, MountContext::Host),
            (
                "/data/child",
                MountContext::Host,
                MountContext::Namespace("installer".to_owned()),
            ),
            (
                "/data/child",
                MountContext::TargetRoot(PathBuf::from("/target-a")),
                MountContext::TargetRoot(PathBuf::from("/target-b")),
            ),
        ] {
            let mut fragment = mount_fragment(
                subject,
                vec![
                    MountAction::Mount {
                        source: MountSource::Tmpfs,
                        target: PathBuf::from(child_path),
                        options: Vec::new(),
                        context: child_context,
                    },
                    MountAction::Mount {
                        source: MountSource::Tmpfs,
                        target: PathBuf::from("/data"),
                        options: Vec::new(),
                        context: parent_context,
                    },
                ],
            );
            // The conservative global mount resource still requires explicit coordination.
            assert!(matches!(
                compose_plan(&current, vec![fragment.clone()]),
                Err(CompositionError::AmbiguousResourceOrder {
                    resource: ActionResource::RuntimeMountState,
                    ..
                })
            ));
            fragment.dependencies.push(FragmentDependency {
                before: LocalActionId::new(0),
                after: LocalActionId::new(1),
                reason: DependencyReason::ResourceCoordination,
            });
            let plan = compose_plan(&current, vec![fragment])
                .unwrap_or_else(|error| panic!("compose independent mount targets: {error}"));
            assert!(
                plan.dependencies()
                    .iter()
                    .all(|dependency| dependency.reason != DependencyReason::MountOrder)
            );
        }
    }

    /// Builds a baseline containing one existing volume group.
    fn current_with_vg() -> (CurrentState, NodeId) {
        let vg = NodeId::new();
        let mut graph = storage_core::model::NodeGraph::new();
        graph
            .insert_new(
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
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
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

    /// Builds a baseline with an ext4 filesystem backed by one LV.
    fn current_with_lv_and_filesystem() -> (CurrentState, NodeId, NodeId) {
        let (mut current, vg) = current_with_vg();
        let lv = NodeId::new();
        let filesystem = NodeId::new();
        current
            .graph
            .insert_new(
                lv,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::LvmLv(LvmLv {
                            name: "data".to_owned(),
                            kind: LvmLvKind::Linear,
                        }),
                        size: Some(Bytes::new(100)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        current
            .graph
            .insert_new(
                filesystem,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Filesystem {
                            kind: FilesystemKind::Ext4,
                            label: Some("data".to_owned()),
                        },
                        size: Some(Bytes::new(100)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        current.graph.insert_dependency(Dependency {
            from: vg,
            to: lv,
            kind: DependencyKind::Provides,
        });
        current.graph.insert_dependency(Dependency {
            from: lv,
            to: filesystem,
            kind: DependencyKind::Backs,
        });
        (current, lv, filesystem)
    }

    /// Builds a resize fragment with the requested semantic order.
    fn resize_fragment(lv: NodeId, filesystem: NodeId, filesystem_first: bool) -> PlanFragment {
        let check = LocalActionId::new(0);
        let resize_lv = LocalActionId::new(1);
        let resize_filesystem = LocalActionId::new(2);
        let (before, after) = if filesystem_first {
            (resize_filesystem, resize_lv)
        } else {
            (resize_lv, resize_filesystem)
        };
        PlanFragment {
            actions: vec![
                FragmentAction {
                    id: check,
                    operation: ProviderAction::Filesystem(FilesystemAction::Check {
                        filesystem,
                        force: true,
                    }),
                },
                FragmentAction {
                    id: resize_lv,
                    operation: ProviderAction::Lvm(LvmAction::ResizeLv {
                        lv,
                        new_size: Bytes::new(60),
                    }),
                },
                FragmentAction {
                    id: resize_filesystem,
                    operation: ProviderAction::Filesystem(FilesystemAction::Resize {
                        filesystem,
                        new_size: Bytes::new(60),
                    }),
                },
            ],
            dependencies: vec![
                FragmentDependency {
                    before: check,
                    after: before,
                    reason: DependencyReason::RequestedOrder,
                },
                FragmentDependency {
                    before,
                    after,
                    reason: DependencyReason::ResizeSafety,
                },
            ],
            external_requirements: Vec::new(),
            origin: origin(lv, "shrink filesystem and LV"),
            diagnostics: Vec::new(),
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
        let simulation = crate::simulate_plan(&current, &plan)
            .unwrap_or_else(|error| panic!("simulate cross-provider plan: {error}"));
        let goal = DesiredState {
            graph: simulation.desired.graph.clone(),
            mounts: MountSpec::default(),
            boot: BootSpec::Unmanaged,
            intents: Vec::new(),
        };
        let report = crate::simulate_plan_for_goal(&current, &plan, &goal)
            .unwrap_or_else(|error| panic!("report conditional creation: {error}"));
        assert!(
            report
                .goal_checks
                .iter()
                .all(|check| check.status == GoalStatus::Satisfied)
        );
        let expected: Vec<_> = report
            .simulation
            .steps
            .iter()
            .flat_map(|step| {
                step.checks
                    .iter()
                    .filter_map(move |check| match &check.status {
                        crate::CheckStatus::Unknown { required_evidence } => {
                            Some(crate::SimulationObligation::Prerequisite {
                                action: step.action,
                                prerequisite: check.prerequisite.clone(),
                                required_evidence: required_evidence.clone(),
                            })
                        }
                        _ => None,
                    })
            })
            .collect();
        assert!(!expected.is_empty());
        assert_eq!(report.unresolved_obligations, expected);
        let protected_report = crate::simulate_plan_for_goal_with_policy(
            &current,
            &plan,
            &goal,
            &[],
            &ProtectedScope {
                nodes: vec![vg, filesystem],
                ..ProtectedScope::default()
            },
        )
        .unwrap_or_else(|error| panic!("report protected metadata and planned node: {error}"));
        assert!(
            protected_report
                .protection_violations
                .iter()
                .any(|violation| violation.aspect == ProtectedAspect::Node(vg))
        );
        assert!(
            protected_report
                .protection_violations
                .iter()
                .any(|violation| violation.aspect == ProtectedAspect::Node(filesystem))
        );
        assert!(
            report
                .unresolved_obligations
                .iter()
                .any(|obligation| matches!(
                    obligation,
                    crate::SimulationObligation::Prerequisite {
                        prerequisite: ActionPrerequisite::DestructiveApproval(_),
                        ..
                    }
                ))
        );
        assert!(!report.is_complete());
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

    /// Mounting or relabeling must not reuse an explicitly deactivated backing LV.
    #[test]
    fn rejects_mount_and_label_after_backing_deactivation() {
        let (current, lv, filesystem) = current_with_lv_and_filesystem();
        for operation in [
            ProviderAction::Mount(MountAction::Mount {
                source: MountSource::Filesystem(filesystem),
                target: PathBuf::from("/data"),
                options: vec!["ro".to_owned()],
                context: MountContext::Host,
            }),
            ProviderAction::Filesystem(FilesystemAction::SetLabel {
                filesystem,
                label: Some("new-label".to_owned()),
            }),
        ] {
            let mut fragment = PlanFragment {
                actions: vec![
                    FragmentAction {
                        id: LocalActionId::new(0),
                        operation: ProviderAction::Lvm(LvmAction::SetActivation {
                            target: lv,
                            active: false,
                        }),
                    },
                    FragmentAction {
                        id: LocalActionId::new(1),
                        operation,
                    },
                ],
                dependencies: vec![FragmentDependency {
                    before: LocalActionId::new(0),
                    after: LocalActionId::new(1),
                    reason: DependencyReason::RequestedOrder,
                }],
                external_requirements: Vec::new(),
                origin: origin(filesystem, "endpoint consumer"),
                diagnostics: Vec::new(),
            };
            assert!(matches!(
                compose_plan(&current, vec![fragment.clone()]),
                Err(CompositionError::ActionRejected { .. })
            ));
            fragment.actions[0].operation = ProviderAction::Lvm(LvmAction::SetActivation {
                target: lv,
                active: true,
            });
            fragment.dependencies.clear();
            assert!(matches!(
                compose_plan(&current, vec![fragment.clone()]),
                Err(CompositionError::AmbiguousResourceOrder {
                    resource: ActionResource::Node(node), ..
                }) if node == lv
            ));
            fragment.dependencies.push(FragmentDependency {
                before: LocalActionId::new(0),
                after: LocalActionId::new(1),
                reason: DependencyReason::RequestedOrder,
            });
            let plan = compose_plan(&current, vec![fragment])
                .unwrap_or_else(|error| panic!("activate before endpoint consumer: {error}"));
            assert_eq!(plan.ordered_actions().len(), 2);
        }
    }

    /// Shrinking the lower LV before its filesystem is rejected.
    #[test]
    fn rejects_backing_shrink_before_filesystem_shrink() {
        let (current, lv, filesystem) = current_with_lv_and_filesystem();
        let fragment = resize_fragment(lv, filesystem, false);

        assert!(matches!(
            compose_plan(&current, vec![fragment]),
            Err(CompositionError::ActionRejected { .. })
        ));
    }

    /// Shrinking the filesystem first leaves a safe intermediate state.
    #[test]
    fn accepts_filesystem_shrink_before_backing_shrink() {
        let (current, lv, filesystem) = current_with_lv_and_filesystem();
        let fragment = resize_fragment(lv, filesystem, true);

        let plan = compose_plan(&current, vec![fragment])
            .unwrap_or_else(|error| panic!("compose safe shrink plan: {error}"));
        let ordered = plan.ordered_actions();

        assert!(matches!(
            ordered[0].operation,
            ProviderAction::Filesystem(FilesystemAction::Check { .. })
        ));
        assert!(matches!(
            ordered[1].operation,
            ProviderAction::Filesystem(FilesystemAction::Resize { .. })
        ));
        assert!(matches!(
            ordered[2].operation,
            ProviderAction::Lvm(LvmAction::ResizeLv { .. })
        ));
    }

    /// A mounted filesystem can be shrunk only through an explicitly ordered offline recipe.
    #[test]
    fn composes_unmount_check_shrink_and_remount() {
        let (mut current, lv, filesystem) = current_with_lv_and_filesystem();
        current
            .mounts
            .entries
            .push(storage_core::model::ObservedMount {
                source: MountSource::Filesystem(filesystem),
                target: PathBuf::from("/data"),
                options: vec!["rw".to_owned()],
                context: MountContext::Host,
            });
        let baseline = current.clone();
        let mut fragment = resize_fragment(lv, filesystem, true);
        fragment.actions.extend([
            FragmentAction {
                id: LocalActionId::new(3),
                operation: ProviderAction::Mount(MountAction::Unmount {
                    target: PathBuf::from("/data"),
                    context: MountContext::Host,
                }),
            },
            FragmentAction {
                id: LocalActionId::new(4),
                operation: ProviderAction::Mount(MountAction::Mount {
                    source: MountSource::Filesystem(filesystem),
                    target: PathBuf::from("/data"),
                    options: vec!["rw".to_owned()],
                    context: MountContext::Host,
                }),
            },
        ]);
        fragment.dependencies.extend([
            FragmentDependency {
                before: LocalActionId::new(3),
                after: LocalActionId::new(0),
                reason: DependencyReason::RequestedOrder,
            },
            FragmentDependency {
                before: LocalActionId::new(1),
                after: LocalActionId::new(4),
                reason: DependencyReason::RequestedOrder,
            },
        ]);
        fragment.external_requirements.extend([
            ExternalRequirement {
                action: LocalActionId::new(0),
                prerequisite: ActionPrerequisite::FilesystemUnmounted(filesystem),
            },
            ExternalRequirement {
                action: LocalActionId::new(2),
                prerequisite: ActionPrerequisite::FilesystemCheckPassed(filesystem),
            },
            ExternalRequirement {
                action: LocalActionId::new(2),
                prerequisite: ActionPrerequisite::FilesystemMinimumSize {
                    filesystem,
                    target: Bytes::new(60),
                },
            },
            ExternalRequirement {
                action: LocalActionId::new(1),
                prerequisite: ActionPrerequisite::DependentsFitWithin {
                    node: lv,
                    size: Bytes::new(60),
                },
            },
        ]);
        let plan = compose_plan(&current, vec![fragment])
            .unwrap_or_else(|error| panic!("compose mounted ext4 shrink: {error}"));
        let ids = plan
            .ordered_actions()
            .iter()
            .map(|action| action.local_id.get())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![3, 0, 2, 1, 4]);
        let simulation = crate::simulate_plan(&current, &plan)
            .unwrap_or_else(|error| panic!("replay mounted ext4 shrink: {error}"));
        assert_eq!(
            simulation.desired.mounts.runtime.as_slice(),
            DesiredState::from_current(&current).mounts.runtime
        );
        assert_eq!(
            simulation
                .desired
                .graph
                .node(&filesystem)
                .map(|node| node.kind.size),
            Some(Some(Bytes::new(60)))
        );
        assert_eq!(current, baseline);
    }

    /// A sequential replay must not allow reordering checks with mount or backing changes.
    #[test]
    fn rejects_unordered_mount_and_filesystem_check() {
        let (current, lv, filesystem) = current_with_lv_and_filesystem();
        let mut fragment = PlanFragment {
            actions: vec![
                FragmentAction {
                    id: LocalActionId::new(0),
                    operation: ProviderAction::Filesystem(FilesystemAction::Check {
                        filesystem,
                        force: true,
                    }),
                },
                FragmentAction {
                    id: LocalActionId::new(1),
                    operation: ProviderAction::Mount(MountAction::Mount {
                        source: MountSource::Filesystem(filesystem),
                        target: PathBuf::from("/data"),
                        options: vec!["rw".to_owned()],
                        context: MountContext::Host,
                    }),
                },
            ],
            dependencies: Vec::new(),
            external_requirements: Vec::new(),
            origin: origin(filesystem, "check then mount"),
            diagnostics: Vec::new(),
        };
        assert!(matches!(
            compose_plan(&current, vec![fragment.clone()]),
            Err(CompositionError::AmbiguousResourceOrder {
                resource: ActionResource::RuntimeMountState,
                ..
            })
        ));
        fragment.dependencies.push(FragmentDependency {
            before: LocalActionId::new(0),
            after: LocalActionId::new(1),
            reason: DependencyReason::RequestedOrder,
        });
        compose_plan(&current, vec![fragment.clone()])
            .unwrap_or_else(|error| panic!("compose ordered check and mount: {error}"));
        fragment.actions[1].operation = ProviderAction::Lvm(LvmAction::ResizeLv {
            lv,
            new_size: Bytes::new(200),
        });
        fragment.dependencies.clear();
        assert!(matches!(compose_plan(&current, vec![fragment.clone()]),
            Err(CompositionError::AmbiguousResourceOrder { resource: ActionResource::Node(node), .. }) if node == lv
        ));
        fragment.dependencies.push(FragmentDependency {
            before: LocalActionId::new(0),
            after: LocalActionId::new(1),
            reason: DependencyReason::RequestedOrder,
        });
        compose_plan(&current, vec![fragment])
            .unwrap_or_else(|error| panic!("compose ordered check and backing resize: {error}"));
    }

    /// Builds a linear LV without consumers for allocation-transition tests.
    fn current_with_linear_lv() -> (CurrentState, NodeId, NodeId) {
        let (mut current, vg) = current_with_vg();
        let lv = NodeId::new();
        current
            .graph
            .insert_new(
                lv,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::LvmLv(LvmLv {
                            name: "old".to_owned(),
                            kind: LvmLvKind::Linear,
                        }),
                        size: Some(Bytes::new(100)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        current.graph.insert_dependency(Dependency {
            from: vg,
            to: lv,
            kind: DependencyKind::Provides,
        });
        (current, vg, lv)
    }

    /// Growing an existing LV and creating another share the remaining free extents.
    #[test]
    fn rejects_growth_and_creation_over_the_shared_capacity() {
        let (current, vg, lv) = current_with_linear_lv();
        let fragment = PlanFragment {
            actions: vec![
                FragmentAction {
                    id: LocalActionId::new(0),
                    operation: ProviderAction::Lvm(LvmAction::ResizeLv {
                        lv,
                        new_size: Bytes::new(160),
                    }),
                },
                FragmentAction {
                    id: LocalActionId::new(1),
                    operation: ProviderAction::Lvm(LvmAction::CreateLv {
                        vg,
                        planned_node_id: NodeId::new(),
                        name: "new".to_owned(),
                        size: Bytes::new(60),
                        layout: LvmLvKind::Linear,
                    }),
                },
            ],
            dependencies: vec![FragmentDependency {
                before: LocalActionId::new(0),
                after: LocalActionId::new(1),
                reason: DependencyReason::ResourceCoordination,
            }],
            external_requirements: Vec::new(),
            origin: origin(vg, "grow and create LV"),
            diagnostics: Vec::new(),
        };
        for (free, succeeds) in [(40, false), (100, false), (120, true)] {
            let evidence = [CapacityEvidence {
                domain: AllocationDomain::LvmVolumeGroup(vg),
                available: AllocationQuantity::new(free),
            }];
            let result = compose_plan_with_evidence(&current, vec![fragment.clone()], &evidence);
            if succeeds {
                let plan = result
                    .unwrap_or_else(|error| panic!("compose balanced grow and create: {error}"));
                crate::simulate_plan_with_evidence(&current, &plan, &evidence)
                    .unwrap_or_else(|error| panic!("replay balanced grow and create: {error}"));
            } else {
                assert!(matches!(
                    result,
                    Err(CompositionError::ActionRejected { .. })
                ));
            }
        }
    }

    /// Capacity returned by shrink or removal is usable only after that action.
    #[test]
    fn shrink_and_removal_release_capacity_for_later_creation() {
        let (current, vg, lv) = current_with_linear_lv();
        let baseline = current.clone();
        let evidence = [CapacityEvidence {
            domain: AllocationDomain::LvmVolumeGroup(vg),
            available: AllocationQuantity::new(0),
        }];
        for (operation, released) in [
            (
                LvmAction::ResizeLv {
                    lv,
                    new_size: Bytes::new(40),
                },
                60,
            ),
            (LvmAction::RemoveLv { lv }, 100),
        ] {
            let mut fragment = PlanFragment {
                actions: vec![
                    FragmentAction {
                        id: LocalActionId::new(0),
                        operation: ProviderAction::Lvm(operation),
                    },
                    FragmentAction {
                        id: LocalActionId::new(1),
                        operation: ProviderAction::Lvm(LvmAction::CreateLv {
                            vg,
                            planned_node_id: NodeId::new(),
                            name: "new".to_owned(),
                            size: Bytes::new(released),
                            layout: LvmLvKind::Linear,
                        }),
                    },
                ],
                dependencies: vec![FragmentDependency {
                    before: LocalActionId::new(0),
                    after: LocalActionId::new(1),
                    reason: DependencyReason::ResourceCoordination,
                }],
                external_requirements: Vec::new(),
                origin: origin(vg, "release then allocate extents"),
                diagnostics: Vec::new(),
            };
            fragment.external_requirements.push(ExternalRequirement {
                action: LocalActionId::new(1),
                prerequisite: ActionPrerequisite::CapacityAvailable {
                    domain: AllocationDomain::LvmVolumeGroup(vg),
                    amount: Some(AllocationQuantity::new(released)),
                },
            });
            let plan = compose_plan_with_evidence(&current, vec![fragment.clone()], &evidence)
                .unwrap_or_else(|error| panic!("compose released capacity reuse: {error}"));
            let simulation = crate::simulate_plan_with_evidence(&current, &plan, &evidence)
                .unwrap_or_else(|error| panic!("replay released capacity reuse: {error}"));
            let mut replayed = TestSimulation::from_current(&current)
                .with_capacity_evidence(&evidence)
                .unwrap_or_else(|error| panic!("initialize replay evidence: {error}"));
            for action in plan.ordered_actions() {
                replayed
                    .simulate_action(&action.operation)
                    .unwrap_or_else(|error| panic!("replay allocation action: {error}"));
            }
            assert_eq!(replayed.desired, simulation.desired);
            let checks =
                replayed.evaluate_prerequisites(&[ActionPrerequisite::CapacityAvailable {
                    domain: AllocationDomain::LvmVolumeGroup(vg),
                    amount: Some(AllocationQuantity::new(1)),
                }]);
            assert!(matches!(
                checks[0].status,
                crate::CheckStatus::Violated { .. }
            ));
            fragment.dependencies[0].before = LocalActionId::new(1);
            fragment.dependencies[0].after = LocalActionId::new(0);
            assert!(matches!(
                compose_plan_with_evidence(&current, vec![fragment], &evidence),
                Err(CompositionError::ActionRejected { .. })
            ));
            assert_eq!(current, baseline);
        }
    }

    /// A created LV is accounted from the predecessor projection, without double credit on removal.
    #[test]
    fn creation_resize_and_removal_restore_capacity_exactly_once() {
        let (current, vg) = current_with_vg();
        let lv = NodeId::new();
        let operations = [
            LvmAction::CreateLv {
                vg,
                planned_node_id: lv,
                name: "temporary".to_owned(),
                size: Bytes::new(100),
                layout: LvmLvKind::Linear,
            },
            LvmAction::ResizeLv {
                lv,
                new_size: Bytes::new(40),
            },
            LvmAction::RemoveLv { lv },
        ];
        let fragment = PlanFragment {
            actions: operations
                .into_iter()
                .zip(0_u32..)
                .map(|(operation, id)| FragmentAction {
                    id: LocalActionId::new(id),
                    operation: ProviderAction::Lvm(operation),
                })
                .collect(),
            dependencies: vec![FragmentDependency {
                before: LocalActionId::new(1),
                after: LocalActionId::new(2),
                reason: DependencyReason::ResourceCoordination,
            }],
            external_requirements: Vec::new(),
            origin: origin(vg, "temporary LV lifecycle"),
            diagnostics: Vec::new(),
        };
        let evidence = [CapacityEvidence {
            domain: AllocationDomain::LvmVolumeGroup(vg),
            available: AllocationQuantity::new(100),
        }];
        let plan = compose_plan_with_evidence(&current, vec![fragment], &evidence)
            .unwrap_or_else(|error| panic!("compose temporary LV: {error}"));
        let simulation = crate::simulate_plan_with_evidence(&current, &plan, &evidence)
            .unwrap_or_else(|error| panic!("replay temporary LV: {error}"));
        assert!(simulation.desired.graph.node(&lv).is_none());
        let mut replayed = TestSimulation::from_current(&current)
            .with_capacity_evidence(&evidence)
            .unwrap_or_else(|error| panic!("initialize replay evidence: {error}"));
        for action in plan.ordered_actions() {
            replayed
                .simulate_action(&action.operation)
                .unwrap_or_else(|error| panic!("replay allocation action: {error}"));
        }
        assert_eq!(replayed.desired, simulation.desired);
        let checks = replayed.evaluate_prerequisites(&[
            ActionPrerequisite::CapacityAvailable {
                domain: AllocationDomain::LvmVolumeGroup(vg),
                amount: Some(AllocationQuantity::new(100)),
            },
            ActionPrerequisite::CapacityAvailable {
                domain: AllocationDomain::LvmVolumeGroup(vg),
                amount: Some(AllocationQuantity::new(101)),
            },
        ]);
        assert!(matches!(
            checks[0].status,
            crate::CheckStatus::Satisfied(crate::CheckEvidence::PredictedEffect)
        ));
        assert!(matches!(
            checks[1].status,
            crate::CheckStatus::Violated { .. }
        ));
    }

    /// Unmodelled layouts and missing geometry cannot leave stale free-capacity evidence usable.
    #[test]
    fn unknown_lvm_allocation_invalidates_the_capacity_forecast() {
        let (current, vg, lv) = current_with_linear_lv();
        for (layout, size, extent) in [
            (LvmLvKind::Thin, Some(Bytes::new(100)), Some(Bytes::new(1))),
            (LvmLvKind::Linear, None, Some(Bytes::new(1))),
            (LvmLvKind::Linear, Some(Bytes::new(100)), None),
            (
                LvmLvKind::Linear,
                Some(Bytes::new(101)),
                Some(Bytes::new(4)),
            ),
        ] {
            let mut baseline = current.clone();
            baseline
                .graph
                .replace_existing(
                    lv,
                    Node {
                        kind: NodeSpec {
                            kind: NodeKind::LvmLv(LvmLv {
                                name: "old".to_owned(),
                                kind: layout.clone(),
                            }),
                            size,
                        },
                        size: NodeFacts::default(),
                    },
                )
                .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
            baseline
                .graph
                .replace_existing(
                    vg,
                    Node {
                        kind: NodeSpec {
                            kind: NodeKind::LvmVg {
                                name: "vg0".to_owned(),
                                extent_size: extent,
                            },
                            size: None,
                        },
                        size: NodeFacts::default(),
                    },
                )
                .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
            let domain = AllocationDomain::LvmVolumeGroup(vg);
            let mut operations = vec![
                LvmAction::ResizeLv {
                    lv,
                    new_size: Bytes::new(160),
                },
                LvmAction::RemoveLv { lv },
            ];
            if layout != LvmLvKind::Linear || extent.is_none() {
                operations.push(LvmAction::CreateLv {
                    vg,
                    planned_node_id: NodeId::new(),
                    name: "unknown".to_owned(),
                    size: Bytes::new(4),
                    layout,
                });
            }
            for operation in operations {
                let mut projection = TestSimulation::from_current(&baseline)
                    .with_capacity_evidence(&[CapacityEvidence {
                        domain: domain.clone(),
                        available: AllocationQuantity::new(100),
                    }])
                    .unwrap_or_else(|error| panic!("initialize allocation evidence: {error}"));
                projection
                    .simulate_action(&ProviderAction::Lvm(operation))
                    .unwrap_or_else(|error| panic!("simulate unknown allocation: {error}"));
                let later = projection
                    .simulate_action(&ProviderAction::Lvm(LvmAction::CreateLv {
                        vg,
                        planned_node_id: NodeId::new(),
                        name: "later".to_owned(),
                        size: Bytes::new(4),
                        layout: LvmLvKind::Linear,
                    }))
                    .unwrap_or_else(|error| {
                        panic!("simulate allocation after unknown mutation: {error}")
                    });
                assert!(later.checks.iter().any(|check| matches!(
                    check,
                    crate::PrerequisiteCheck {
                        prerequisite: ActionPrerequisite::CapacityAvailable { .. },
                        status: crate::CheckStatus::Unknown { .. }
                    }
                )));
            }
        }
    }

    /// Overflow in returned capacity rejects the whole effect, including node deletion.
    #[test]
    fn overflowing_capacity_release_leaves_the_projection_unchanged() {
        let (mut current, vg, lv) = current_with_linear_lv();
        let mut node = current
            .graph
            .node(&lv)
            .cloned()
            .unwrap_or_else(|| panic!("LV is absent"));
        node.kind.size = Some(Bytes::new(u64::MAX));
        current
            .graph
            .replace_existing(lv, node)
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let mut projection = TestSimulation::from_current(&current)
            .with_capacity_evidence(&[CapacityEvidence {
                domain: AllocationDomain::LvmVolumeGroup(vg),
                available: AllocationQuantity::new(1),
            }])
            .unwrap_or_else(|error| panic!("initialize overflow evidence: {error}"));
        let before = projection.clone();
        assert!(matches!(
            projection.simulate_action(&ProviderAction::Lvm(LvmAction::RemoveLv { lv })),
            Err(ActionSimulationError::Effect(
                crate::EffectApplicationError::CapacityOverflow(_)
            ))
        ));
        assert_eq!(projection, before);
    }

    /// Cumulative reservations reject two locally valid allocations over capacity.
    #[test]
    fn rejects_cumulative_volume_group_overallocation() {
        let (current, vg) = current_with_vg();
        let fragment = PlanFragment {
            actions: ["one", "two"]
                .into_iter()
                .enumerate()
                .map(|(index, name)| FragmentAction {
                    id: LocalActionId::new(index as u32),
                    operation: ProviderAction::Lvm(LvmAction::CreateLv {
                        vg,
                        planned_node_id: NodeId::new(),
                        name: name.to_owned(),
                        size: Bytes::new(60),
                        layout: LvmLvKind::Linear,
                    }),
                })
                .collect(),
            dependencies: Vec::new(),
            external_requirements: Vec::new(),
            origin: origin(vg, "allocate two LVs"),
            diagnostics: Vec::new(),
        };
        let evidence = [CapacityEvidence {
            domain: AllocationDomain::LvmVolumeGroup(vg),
            available: AllocationQuantity::new(100),
        }];

        let error = compose_plan_with_evidence(&current, vec![fragment], &evidence)
            .err()
            .unwrap_or_else(|| panic!("over-allocation must fail"));
        let CompositionError::ActionRejected { action, checks } = error else {
            panic!("expected prerequisite rejection, got {error:?}");
        };
        assert_eq!(action, ActionId::new(1));
        assert!(checks.iter().any(|check| matches!(
            (&check.prerequisite, &check.status),
            (
                ActionPrerequisite::CapacityAvailable { .. },
                crate::CheckStatus::Violated { .. }
            ),
        )));
        assert!(
            checks
                .iter()
                .any(|check| matches!(check.status, crate::CheckStatus::Satisfied(_)))
        );
    }

    /// Conflicting writes need a provider- or recipe-owned semantic order.
    #[test]
    fn rejects_unordered_resource_writes() {
        let (current, vg) = current_with_vg();
        let first = LocalActionId::new(0);
        let second = LocalActionId::new(1);
        let mut fragment = PlanFragment {
            actions: vec![
                FragmentAction {
                    id: first,
                    operation: ProviderAction::Lvm(LvmAction::SetActivation {
                        target: vg,
                        active: false,
                    }),
                },
                FragmentAction {
                    id: second,
                    operation: ProviderAction::Lvm(LvmAction::SetActivation {
                        target: vg,
                        active: true,
                    }),
                },
            ],
            dependencies: Vec::new(),
            external_requirements: Vec::new(),
            origin: origin(vg, "ordered activation"),
            diagnostics: Vec::new(),
        };

        assert!(matches!(
            compose_plan(&current, vec![fragment.clone()]),
            Err(CompositionError::AmbiguousResourceOrder { .. })
        ));

        fragment.dependencies.push(FragmentDependency {
            before: first,
            after: second,
            reason: DependencyReason::ResourceCoordination,
        });
        assert!(compose_plan(&current, vec![fragment]).is_ok());
    }

    /// Explicit edges resolve shared VG writes and reject malformed or cyclic ordering.
    #[test]
    fn cross_fragment_ordering_is_validated_before_replay() {
        let (current, vg) = current_with_vg();
        let baseline = current.clone();
        let fragments: Vec<_> = ["data", "logs"]
            .into_iter()
            .map(|name| PlanFragment {
                actions: vec![FragmentAction {
                    id: LocalActionId::new(7),
                    operation: ProviderAction::Lvm(LvmAction::CreateLv {
                        vg,
                        planned_node_id: NodeId::new(),
                        name: name.to_owned(),
                        size: Bytes::new(20),
                        layout: LvmLvKind::Linear,
                    }),
                }],
                dependencies: Vec::new(),
                external_requirements: Vec::new(),
                origin: origin(vg, "create LV"),
                diagnostics: Vec::new(),
            })
            .collect();
        let capacity = [CapacityEvidence {
            domain: AllocationDomain::LvmVolumeGroup(vg),
            available: AllocationQuantity::new(100),
        }];
        let first = FragmentActionRef {
            fragment: FragmentId::new(0),
            action: LocalActionId::new(7),
        };
        let second = FragmentActionRef {
            fragment: FragmentId::new(1),
            action: LocalActionId::new(7),
        };
        let edge = FragmentOrdering {
            before: second,
            after: first,
            reason: DependencyReason::RequestedOrder,
        };
        assert!(matches!(
            compose_plan_with_evidence(&current, fragments.clone(), &capacity),
            Err(CompositionError::AmbiguousResourceOrder { .. }),
        ));
        let plan =
            compose_plan_with_ordering(&current, fragments.clone(), &[edge.clone()], &capacity)
                .unwrap_or_else(|error| panic!("compose ordered LVs: {error}"));
        assert_eq!(
            plan.ordered_actions()
                .iter()
                .map(|action| action.id)
                .collect::<Vec<_>>(),
            vec![ActionId::new(1), ActionId::new(0)]
        );
        let json =
            serde_json::to_string(&edge).unwrap_or_else(|error| panic!("encode edge: {error}"));
        assert_eq!(
            serde_json::from_str::<FragmentOrdering>(&json)
                .unwrap_or_else(|error| panic!("decode edge: {error}")),
            edge
        );
        for missing in [
            FragmentActionRef {
                fragment: FragmentId::new(2),
                ..first
            },
            FragmentActionRef {
                action: LocalActionId::new(0),
                ..first
            },
        ] {
            assert_eq!(
                compose_plan_with_ordering(
                    &current,
                    fragments.clone(),
                    &[FragmentOrdering {
                        before: missing,
                        ..edge.clone()
                    }],
                    &capacity
                ),
                Err(CompositionError::UnknownOrderingAction(missing))
            );
        }
        assert_eq!(
            compose_plan_with_ordering(
                &current,
                fragments.clone(),
                &[FragmentOrdering {
                    before: first,
                    ..edge.clone()
                }],
                &capacity
            ),
            Err(CompositionError::SelfDependency(ActionId::new(0)))
        );
        assert_eq!(
            compose_plan_with_ordering(
                &current,
                fragments,
                &[
                    edge.clone(),
                    FragmentOrdering {
                        before: first,
                        after: second,
                        ..edge
                    }
                ],
                &capacity
            ),
            Err(CompositionError::DependencyCycle)
        );
        assert_eq!(current, baseline);
    }

    /// User ordering cannot place a consumer before its mandatory planned input.
    #[test]
    fn cross_fragment_ordering_cannot_reverse_creation() {
        let (current, vg) = current_with_vg();
        let lv = NodeId::new();
        let fragments = vec![
            PlanFragment {
                actions: vec![FragmentAction {
                    id: LocalActionId::new(0),
                    operation: ProviderAction::Lvm(LvmAction::CreateLv {
                        vg,
                        planned_node_id: lv,
                        name: "data".to_owned(),
                        size: Bytes::new(20),
                        layout: LvmLvKind::Linear,
                    }),
                }],
                dependencies: Vec::new(),
                external_requirements: Vec::new(),
                origin: origin(lv, "create LV"),
                diagnostics: Vec::new(),
            },
            PlanFragment {
                actions: vec![FragmentAction {
                    id: LocalActionId::new(0),
                    operation: ProviderAction::Lvm(LvmAction::SetActivation {
                        target: lv,
                        active: true,
                    }),
                }],
                dependencies: Vec::new(),
                external_requirements: Vec::new(),
                origin: origin(lv, "activate LV"),
                diagnostics: Vec::new(),
            },
        ];
        let edge = FragmentOrdering {
            before: FragmentActionRef {
                fragment: FragmentId::new(1),
                action: LocalActionId::new(0),
            },
            after: FragmentActionRef {
                fragment: FragmentId::new(0),
                action: LocalActionId::new(0),
            },
            reason: DependencyReason::RequestedOrder,
        };
        assert_eq!(
            compose_plan_with_ordering(&current, fragments, &[edge], &[]),
            Err(CompositionError::DependencyCycle)
        );
    }

    /// Partition mutations conflict with creation in their shared metadata domain.
    #[test]
    fn partition_mutations_require_order_with_creation_in_the_same_table() {
        let (mut current, _) = current_with_vg();
        let table = NodeId::new();
        let partition = NodeId::new();
        current
            .graph
            .insert_new(
                table,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::PartitionTable(storage_core::model::PartitionTable::Gpt),
                        size: Some(Bytes::new(1024)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let attributes = storage_core::model::PartitionAttributes::default();
        let number = std::num::NonZeroU32::new(1)
            .unwrap_or_else(|| panic!("partition number must be nonzero"));
        current
            .graph
            .insert_new(
                partition,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Partition {
                            number,
                            offset: Bytes::new(0),
                            role: None,
                            attributes,
                        },
                        size: Some(Bytes::new(100)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        current.graph.insert_dependency(Dependency {
            from: table,
            to: partition,
            kind: DependencyKind::Contains {
                offset: Bytes::new(0),
            },
        });
        let baseline = current.clone();
        for operation in [
            PartitionAction::Delete { partition },
            PartitionAction::Resize {
                partition,
                new_size: Bytes::new(60),
            },
            PartitionAction::SetAttributes {
                partition,
                attributes,
            },
        ] {
            let mut fragment = PlanFragment {
                actions: vec![
                    FragmentAction {
                        id: LocalActionId::new(0),
                        operation: ProviderAction::Partition(operation),
                    },
                    FragmentAction {
                        id: LocalActionId::new(1),
                        operation: ProviderAction::Partition(PartitionAction::Create {
                            table,
                            planned_node_id: NodeId::new(),
                            number: std::num::NonZeroU32::new(2)
                                .unwrap_or_else(|| panic!("partition number must be nonzero")),
                            offset: Bytes::new(200),
                            size: Bytes::new(100),
                            role: None,
                            attributes,
                        }),
                    },
                ],
                dependencies: Vec::new(),
                external_requirements: Vec::new(),
                origin: origin(table, "mutate and create partitions"),
                diagnostics: Vec::new(),
            };
            assert!(matches!(
                compose_plan(&current, vec![fragment.clone()]),
                Err(CompositionError::AmbiguousResourceOrder {
                    resource: ActionResource::PartitionTable(id), ..
                }) if id == table
            ));
            let mut independent_current = current.clone();
            let other_table = NodeId::new();
            independent_current
                .graph
                .insert_new(
                    other_table,
                    current
                        .graph
                        .node(&table)
                        .cloned()
                        .unwrap_or_else(|| panic!("existing partition table is absent")),
                )
                .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
            let mut independent_fragment = fragment.clone();
            if let ProviderAction::Partition(PartitionAction::Create { table, .. }) =
                &mut independent_fragment.actions[1].operation
            {
                *table = other_table;
            }
            compose_plan(&independent_current, vec![independent_fragment])
                .unwrap_or_else(|error| panic!("compose changes in different tables: {error}"));
            fragment.dependencies.push(FragmentDependency {
                before: LocalActionId::new(0),
                after: LocalActionId::new(1),
                reason: DependencyReason::ResourceCoordination,
            });
            let plan = compose_plan(&current, vec![fragment])
                .unwrap_or_else(|error| panic!("compose ordered partition changes: {error}"));
            crate::simulate_plan(&current, &plan)
                .unwrap_or_else(|error| panic!("replay ordered partition changes: {error}"));
            assert_eq!(current, baseline);
        }
    }

    /// Explicit checks remain in the plan despite having no graph effect.
    #[test]
    fn explicit_check_intent_requires_and_retains_matching_action() {
        let (current, _lv, filesystem) = current_with_lv_and_filesystem();
        let goal = DesiredState {
            graph: current.graph.clone(),
            mounts: MountSpec::default(),
            boot: BootSpec::Unmanaged,
            intents: vec![OperationIntent::Check {
                target: filesystem,
                force: true,
            }],
        };
        assert_eq!(
            compose_plan_for_goal(&current, &goal, Vec::new()),
            Err(CompositionError::UncoveredIntent { index: 0 })
        );
        let mut fragment = PlanFragment {
            actions: vec![FragmentAction {
                id: LocalActionId::new(0),
                operation: ProviderAction::Filesystem(FilesystemAction::Check {
                    filesystem,
                    force: false,
                }),
            }],
            dependencies: Vec::new(),
            external_requirements: Vec::new(),
            origin: FragmentOrigin::Intent { index: 0 },
            diagnostics: Vec::new(),
        };
        assert_eq!(
            compose_plan_for_goal(&current, &goal, vec![fragment.clone()]),
            Err(CompositionError::UncoveredIntent { index: 0 })
        );
        fragment.actions[0].operation = ProviderAction::Filesystem(FilesystemAction::Check {
            filesystem,
            force: true,
        });

        let plan = compose_plan_for_goal(&current, &goal, vec![fragment])
            .unwrap_or_else(|error| panic!("compose explicit check: {error}"));

        let baseline = current.clone();
        let requested = goal.clone();
        let report = crate::simulate_plan_for_goal(&current, &plan, &goal)
            .unwrap_or_else(|error| panic!("report explicit check: {error}"));
        assert_eq!(
            report.intent_coverage,
            vec![crate::IntentCoverage {
                index: 0,
                actions: vec![ActionId::new(0)],
                covered: true,
            }]
        );
        assert!(report.is_complete());
        let analysis = crate::analyse_plan(
            &current,
            &plan,
            &goal.intents,
            &[],
            &ProtectedScope::default(),
        )
        .unwrap_or_else(|error| panic!("analyse explicit check: {error}"));
        assert_eq!(analysis.simulation.desired.intents, goal.intents);
        assert_eq!(analysis.intent_coverage, report.intent_coverage);
        assert!(analysis.goal_checks.is_empty());
        assert_eq!(
            crate::analyse_plan(&current, &plan, &[], &[], &ProtectedScope::default()),
            Err(PlanSimulationError::UnknownIntentOrigin {
                action: ActionId::new(0),
                index: 0
            }),
        );
        let mut wrong_goal = goal.clone();
        wrong_goal.intents = vec![OperationIntent::Repair {
            target: filesystem,
            force: true,
        }];
        let wrong_report = crate::simulate_plan_for_goal(&current, &plan, &wrong_goal)
            .unwrap_or_else(|error| panic!("report mismatched intent: {error}"));
        assert!(!wrong_report.intent_coverage[0].covered);
        assert!(!wrong_report.is_complete());
        wrong_goal.intents.clear();
        assert_eq!(
            crate::simulate_plan_for_goal(&current, &plan, &wrong_goal),
            Err(PlanSimulationError::UnknownIntentOrigin {
                action: ActionId::new(0),
                index: 0
            })
        );
        assert_eq!(current, baseline);
        assert_eq!(goal, requested);

        assert_eq!(plan.actions().len(), 1);
        assert!(
            plan.actions()[0]
                .informational_contract
                .predicted_effect
                .graph
                .is_empty()
        );
    }

    /// A transition-safe but incomplete plan cannot pass final goal validation.
    #[test]
    fn rejects_plan_that_does_not_reach_requested_graph() {
        let (current, _vg) = current_with_vg();
        let mut goal_graph = current.graph.clone();
        goal_graph
            .insert_new(
                NodeId::new(),
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: Some(Bytes::new(1)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let goal = DesiredState {
            graph: goal_graph,
            mounts: MountSpec::default(),
            boot: BootSpec::Unmanaged,
            intents: Vec::new(),
        };

        assert_eq!(
            compose_plan_for_goal(&current, &goal, Vec::new()),
            Err(CompositionError::GoalNotReached(GoalAspect::Graph))
        );
    }
    /// Named clean-check outputs must be bound before a shrink consumer is simulated.
    #[test]
    fn binds_check_outputs_before_first_replay_and_preserves_explicit_phases() {
        let (current, _lv, filesystem) = current_with_lv_and_filesystem();
        let baseline = current.clone();
        let mut fragment = PlanFragment {
            actions: vec![
                FragmentAction {
                    id: LocalActionId::new(0),
                    operation: ProviderAction::Filesystem(FilesystemAction::Resize {
                        filesystem,
                        new_size: Bytes::new(60),
                    }),
                },
                FragmentAction {
                    id: LocalActionId::new(1),
                    operation: ProviderAction::Filesystem(FilesystemAction::Check {
                        filesystem,
                        force: true,
                    }),
                },
            ],
            dependencies: Vec::new(),
            external_requirements: vec![ExternalRequirement {
                action: LocalActionId::new(0),
                prerequisite: ActionPrerequisite::FilesystemCheckPassed(filesystem),
            }],
            origin: origin(filesystem, "check before shrink"),
            diagnostics: Vec::new(),
        };
        let plan = compose_plan(&current, vec![fragment.clone()])
            .unwrap_or_else(|error| panic!("bind earlier clean check: {error}"));
        assert_eq!(
            plan.ordered_actions()
                .iter()
                .map(|action| action.local_id.get())
                .collect::<Vec<_>>(),
            vec![1, 0]
        );
        assert!(
            plan.dependencies()
                .iter()
                .any(|dependency| dependency.before == ActionId::new(1)
                    && dependency.after == ActionId::new(0)
                    && dependency.reason == DependencyReason::InterfacePreparation)
        );
        crate::simulate_plan(&current, &plan)
            .unwrap_or_else(|error| panic!("replay bound check: {error}"));
        fragment.actions[1].operation = ProviderAction::Filesystem(FilesystemAction::Check {
            filesystem,
            force: false,
        });
        assert!(matches!(
            compose_plan(&current, vec![fragment.clone()]),
            Err(CompositionError::ActionRejected { .. })
        ));
        fragment.actions[1].operation = ProviderAction::Filesystem(FilesystemAction::Check {
            filesystem,
            force: true,
        });
        fragment.dependencies.push(FragmentDependency {
            before: LocalActionId::new(0),
            after: LocalActionId::new(1),
            reason: DependencyReason::RequestedOrder,
        });
        assert!(matches!(
            compose_plan(&current, vec![fragment.clone()]),
            Err(CompositionError::ActionRejected { .. })
        ));
        fragment.dependencies.clear();
        fragment.actions.push(FragmentAction {
            id: LocalActionId::new(2),
            operation: ProviderAction::Filesystem(FilesystemAction::Check {
                filesystem,
                force: true,
            }),
        });
        assert_eq!(
            compose_plan(&current, vec![fragment.clone()]),
            Err(CompositionError::AmbiguousRequirement {
                action: ActionId::new(0)
            })
        );
        fragment.dependencies.extend([
            FragmentDependency {
                before: LocalActionId::new(1),
                after: LocalActionId::new(2),
                reason: DependencyReason::RequestedOrder,
            },
            FragmentDependency {
                before: LocalActionId::new(2),
                after: LocalActionId::new(0),
                reason: DependencyReason::RequestedOrder,
            },
        ]);
        compose_plan(&current, vec![fragment])
            .unwrap_or_else(|error| panic!("bind latest explicitly ordered check: {error}"));
        assert_eq!(current, baseline);
    }

    /// Read-only observations cannot force rejection before a declared activation provider runs.
    #[test]
    fn binds_writable_outputs_before_read_only_consumer() {
        let (mut current, _vg, lv) = current_with_linear_lv();
        let mut node = current
            .graph
            .node(&lv)
            .cloned()
            .unwrap_or_else(|| panic!("LV fixture"));
        node.size.block = Some(storage_core::model::BlockFacts {
            paths: vec![PathBuf::from("/dev/mapper/old")],
            devno: storage_core::model::DeviceNumber {
                major: 253,
                minor: 0,
            },
            geometry: None,
            read_only: Some(true),
        });
        current
            .graph
            .replace_existing(lv, node)
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let baseline = current.clone();
        let fragment = PlanFragment {
            actions: vec![
                FragmentAction {
                    id: LocalActionId::new(0),
                    operation: ProviderAction::Filesystem(FilesystemAction::Create {
                        backing: lv,
                        planned_node_id: NodeId::new(),
                        filesystem: FilesystemKind::Ext4,
                        label: None,
                    }),
                },
                FragmentAction {
                    id: LocalActionId::new(1),
                    operation: ProviderAction::Lvm(LvmAction::SetActivation {
                        target: lv,
                        active: true,
                    }),
                },
            ],
            dependencies: Vec::new(),
            external_requirements: vec![ExternalRequirement {
                action: LocalActionId::new(0),
                prerequisite: ActionPrerequisite::Writable(lv),
            }],
            origin: origin(lv, "activate before mkfs"),
            diagnostics: Vec::new(),
        };
        let plan = compose_plan(&current, vec![fragment])
            .unwrap_or_else(|error| panic!("bind activation before writable check: {error}"));
        let simulation = crate::simulate_plan(&current, &plan)
            .unwrap_or_else(|error| panic!("replay bound activation: {error}"));
        assert_eq!(simulation.steps[0].action, ActionId::new(1));
        assert!(
            simulation.steps[1]
                .checks
                .iter()
                .any(
                    |check| check.prerequisite == ActionPrerequisite::Writable(lv)
                        && check.status
                            == crate::CheckStatus::Satisfied(crate::CheckEvidence::PredictedEffect)
                )
        );
        assert_eq!(current, baseline);
    }

    /// Missing backing geometry remains an explicit obligation even when declared externally.
    #[test]
    fn unknown_external_geometry_is_retained_in_the_report() {
        let (mut current, lv, filesystem) = current_with_lv_and_filesystem();
        let mut node = current
            .graph
            .node(&lv)
            .cloned()
            .unwrap_or_else(|| panic!("LV fixture"));
        node.kind.size = None;
        current
            .graph
            .replace_existing(lv, node)
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let condition = ActionPrerequisite::BackingSizeAtLeast {
            node: filesystem,
            size: Bytes::new(200),
        };
        let mut fragment = PlanFragment {
            actions: vec![FragmentAction {
                id: LocalActionId::new(0),
                operation: ProviderAction::Filesystem(FilesystemAction::Resize {
                    filesystem,
                    new_size: Bytes::new(200),
                }),
            }],
            dependencies: Vec::new(),
            external_requirements: vec![ExternalRequirement {
                action: LocalActionId::new(0),
                prerequisite: condition.clone(),
            }],
            origin: origin(filesystem, "grow with unknown backing geometry"),
            diagnostics: Vec::new(),
        };
        let plan = compose_plan(&current, vec![fragment.clone()])
            .unwrap_or_else(|error| panic!("compose conditional geometry: {error}"));
        let mut goal = DesiredState {
            graph: current.graph.clone(),
            mounts: MountSpec::default(),
            boot: BootSpec::Unmanaged,
            intents: Vec::new(),
        };
        let mut node = goal
            .graph
            .node(&filesystem)
            .cloned()
            .unwrap_or_else(|| panic!("FS fixture"));
        node.kind.size = Some(Bytes::new(200));
        goal.graph
            .replace_existing(filesystem, node)
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let report = crate::simulate_plan_for_goal(&current, &plan, &goal)
            .unwrap_or_else(|error| panic!("report unknown geometry: {error}"));
        assert!(report.unresolved_obligations.iter().any(|obligation| matches!(obligation,
            crate::SimulationObligation::Prerequisite { action, prerequisite, .. } if *action == ActionId::new(0) && *prerequisite == condition)));
        assert!(!report.is_complete());
        fragment.external_requirements[0].prerequisite =
            ActionPrerequisite::FilesystemCheckPassed(filesystem);
        assert_eq!(
            compose_plan(&current, vec![fragment]),
            Err(CompositionError::RequirementNotInContract {
                action: ActionId::new(0)
            })
        );
    }
    /// Restoring a label or activation cannot conceal intermediate protected mutations.
    #[test]
    fn protected_nodes_reject_transient_changes_but_allow_read_only_checks() {
        let (current, lv, filesystem) = current_with_lv_and_filesystem();
        let goal = DesiredState {
            graph: current.graph.clone(),
            mounts: MountSpec::default(),
            boot: BootSpec::Unmanaged,
            intents: Vec::new(),
        };
        let protected = ProtectedScope {
            nodes: vec![filesystem, lv, filesystem],
            ..ProtectedScope::default()
        };
        let mut fragment = PlanFragment {
            actions: vec![
                FragmentAction {
                    id: LocalActionId::new(0),
                    operation: ProviderAction::Filesystem(FilesystemAction::SetLabel {
                        filesystem,
                        label: Some("temporary".to_owned()),
                    }),
                },
                FragmentAction {
                    id: LocalActionId::new(1),
                    operation: ProviderAction::Filesystem(FilesystemAction::SetLabel {
                        filesystem,
                        label: Some("data".to_owned()),
                    }),
                },
            ],
            dependencies: vec![FragmentDependency {
                before: LocalActionId::new(0),
                after: LocalActionId::new(1),
                reason: DependencyReason::RequestedOrder,
            }],
            external_requirements: Vec::new(),
            origin: origin(filesystem, "temporary label"),
            diagnostics: Vec::new(),
        };
        let plan = compose_plan_for_goal(&current, &goal, vec![fragment.clone()])
            .unwrap_or_else(|error| panic!("compose restored label: {error}"));
        let report =
            crate::simulate_plan_for_goal_with_policy(&current, &plan, &goal, &[], &protected)
                .unwrap_or_else(|error| panic!("report protected labels: {error}"));
        assert!(
            report
                .goal_checks
                .iter()
                .all(|check| check.status == GoalStatus::Satisfied)
        );
        assert_eq!(report.protection_violations.len(), 4);
        let analysis = crate::analyse_plan(&current, &plan, &[], &[], &protected)
            .unwrap_or_else(|error| panic!("analyse protected labels: {error}"));
        assert_eq!(analysis.protection_violations, report.protection_violations);
        assert_eq!(
            analysis.unresolved_obligations,
            report.unresolved_obligations
        );
        assert!(!analysis.is_complete());
        for action in [ActionId::new(0), ActionId::new(1)] {
            for node in [filesystem, lv] {
                assert!(
                    report
                        .protection_violations
                        .contains(&crate::ProtectionViolation {
                            action,
                            aspect: ProtectedAspect::Node(node),
                        })
                );
            }
        }
        assert!(!report.is_complete());
        assert_eq!(
            compose_plan_for_goal_with_policy(
                &current,
                &goal,
                vec![fragment.clone()],
                &[],
                &protected
            ),
            Err(CompositionError::ProtectedScopeViolation {
                action: ActionId::new(0),
                aspect: ProtectedAspect::Node(filesystem)
            })
        );
        fragment.actions = vec![FragmentAction {
            id: LocalActionId::new(0),
            operation: ProviderAction::Filesystem(FilesystemAction::Repair {
                filesystem,
                force: true,
            }),
        }];
        fragment.dependencies.clear();
        assert!(matches!(
            compose_plan_for_goal_with_policy(
                &current,
                &goal,
                vec![fragment.clone()],
                &[],
                &protected
            ),
            Err(CompositionError::ProtectedScopeViolation { .. })
        ));
        fragment.actions[0].operation = ProviderAction::Filesystem(FilesystemAction::Check {
            filesystem,
            force: true,
        });
        compose_plan_for_goal_with_policy(&current, &goal, vec![fragment], &[], &protected)
            .unwrap_or_else(|error| panic!("read-only check of protected nodes: {error}"));
        let (current, vg, lv) = current_with_linear_lv();
        let goal = DesiredState {
            graph: current.graph.clone(),
            mounts: MountSpec::default(),
            boot: BootSpec::Unmanaged,
            intents: Vec::new(),
        };
        let fragment = PlanFragment {
            actions: vec![FragmentAction {
                id: LocalActionId::new(0),
                operation: ProviderAction::Lvm(LvmAction::SetActivation {
                    target: vg,
                    active: false,
                }),
            }],
            dependencies: Vec::new(),
            external_requirements: Vec::new(),
            origin: origin(vg, "deactivate group"),
            diagnostics: Vec::new(),
        };
        let protected = ProtectedScope {
            nodes: vec![lv],
            ..ProtectedScope::default()
        };
        assert_eq!(
            compose_plan_for_goal_with_policy(&current, &goal, vec![fragment], &[], &protected),
            Err(CompositionError::ProtectedScopeViolation {
                action: ActionId::new(0),
                aspect: ProtectedAspect::Node(lv)
            })
        );
    }

    /// An automatic resize may predict unknown size while keeping an exact requested size conditional.
    #[test]
    fn goal_composition_retains_unknown_requested_capacity() {
        let (mut current, _) = current_with_vg();
        let disk = NodeId::new();
        let mapping = NodeId::new();
        for (id, kind, size) in [
            (disk, NodeKind::Disk, 1000),
            (
                mapping,
                NodeKind::DmCryptMapping {
                    name: "data".to_owned(),
                },
                100,
            ),
        ] {
            current
                .graph
                .insert_new(
                    id,
                    Node {
                        kind: NodeSpec {
                            kind,
                            size: Some(Bytes::new(size)),
                        },
                        size: NodeFacts::default(),
                    },
                )
                .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        }
        current.graph.insert_dependency(Dependency {
            from: disk,
            to: mapping,
            kind: DependencyKind::Backs,
        });
        let goal = DesiredState {
            graph: current.graph.clone(),
            mounts: MountSpec::default(),
            boot: BootSpec::Unmanaged,
            intents: Vec::new(),
        };
        let fragment = PlanFragment {
            actions: vec![FragmentAction {
                id: LocalActionId::new(0),
                operation: ProviderAction::Luks(LuksAction::Resize {
                    mapping,
                    new_size: None,
                }),
            }],
            dependencies: Vec::new(),
            external_requirements: Vec::new(),
            origin: origin(disk, "automatic mapping resize"),
            diagnostics: Vec::new(),
        };
        let plan = compose_plan_for_goal(&current, &goal, vec![fragment])
            .unwrap_or_else(|error| panic!("compose conditional mapping size: {error}"));
        let report = crate::simulate_plan_for_goal(&current, &plan, &goal)
            .unwrap_or_else(|error| panic!("report mapping size: {error}"));
        assert!(
            report
                .unresolved_obligations
                .iter()
                .any(|obligation| matches!(
                    obligation,
                    crate::SimulationObligation::Goal {
                        aspect: GoalAspect::Graph,
                        ..
                    }
                ))
        );
        assert!(!report.is_complete());
    }
    /// Protected runtime hierarchy and persistent configuration remain separate across contexts.
    #[test]
    fn protected_mounts_enforce_hierarchy_context_and_persistence() {
        let (current, subject) = current_with_vg();
        let goal = DesiredState {
            graph: current.graph.clone(),
            mounts: MountSpec::default(),
            boot: BootSpec::Unmanaged,
            intents: Vec::new(),
        };
        let host = crate::ProtectedMount {
            target: PathBuf::from("/data/child"),
            context: MountContext::Host,
        };
        let protected = ProtectedScope {
            runtime_mounts: vec![host.clone()],
            ..ProtectedScope::default()
        };
        for (target, context, conflict) in [
            ("/data", MountContext::Host, true),
            ("/data/child/deeper", MountContext::Host, true),
            ("/data2", MountContext::Host, false),
            ("/data", MountContext::Namespace("other".to_owned()), false),
        ] {
            let fragment = mount_fragment(
                subject,
                vec![MountAction::Mount {
                    source: MountSource::Tmpfs,
                    target: PathBuf::from(target),
                    options: Vec::new(),
                    context,
                }],
            );
            let result =
                compose_plan_for_goal_with_policy(&current, &goal, vec![fragment], &[], &protected);
            if conflict {
                assert_eq!(
                    result,
                    Err(CompositionError::ProtectedScopeViolation {
                        action: ActionId::new(0),
                        aspect: ProtectedAspect::RuntimeMount(host.clone())
                    })
                );
            } else {
                result.unwrap_or_else(|error| panic!("unrelated mount context or prefix: {error}"));
            }
        }
        let fragment = mount_fragment(
            subject,
            vec![MountAction::Persist {
                entry: storage_core::model::MountEntry {
                    source: MountSource::Tmpfs,
                    target: PathBuf::from("/other"),
                    options: Vec::new(),
                    context: MountContext::Host,
                },
            }],
        );
        compose_plan_for_goal_with_policy(&current, &goal, vec![fragment.clone()], &[], &protected)
            .unwrap_or_else(|error| {
                panic!("persistent write does not alter protected runtime mount: {error}")
            });
        let persistent = ProtectedScope {
            persistent_mounts: vec![host.clone()],
            ..ProtectedScope::default()
        };
        assert_eq!(
            compose_plan_for_goal_with_policy(&current, &goal, vec![fragment], &[], &persistent),
            Err(CompositionError::ProtectedScopeViolation {
                action: ActionId::new(0),
                aspect: ProtectedAspect::PersistentMount(host)
            })
        );
    }

    /// Mount policy paths must obey the same absolute lexical constraints as mount actions.
    #[test]
    fn rejects_ambiguous_protection_paths() {
        let (current, _) = current_with_vg();
        let goal = DesiredState {
            graph: current.graph.clone(),
            mounts: MountSpec::default(),
            boot: BootSpec::Unmanaged,
            intents: Vec::new(),
        };
        for (target, context) in [
            ("relative", MountContext::Host),
            ("/data/../protected", MountContext::Host),
            ("/data\0", MountContext::Host),
            ("/data", MountContext::TargetRoot(PathBuf::from("relative"))),
        ] {
            let mount = crate::ProtectedMount {
                target: PathBuf::from(target),
                context,
            };
            let policy = ProtectedScope {
                runtime_mounts: vec![mount.clone()],
                ..ProtectedScope::default()
            };
            assert_eq!(
                compose_plan_for_goal_with_policy(&current, &goal, Vec::new(), &[], &policy),
                Err(CompositionError::FinalSimulation(
                    PlanSimulationError::InvalidProtectedMount(mount)
                ))
            );
        }
    }
    /// A new filesystem's clean-check output requires its creator's intermediate graph context.
    #[test]
    fn binds_contextual_outputs_for_a_new_filesystem_before_first_consumer_replay() {
        let (current, _vg, lv) = current_with_linear_lv();
        let baseline = current.clone();
        let filesystem = NodeId::new();
        let mut fragment = PlanFragment {
            actions: vec![
                FragmentAction {
                    id: LocalActionId::new(0),
                    operation: ProviderAction::Filesystem(FilesystemAction::Resize {
                        filesystem,
                        new_size: Bytes::new(60),
                    }),
                },
                FragmentAction {
                    id: LocalActionId::new(1),
                    operation: ProviderAction::Filesystem(FilesystemAction::Create {
                        backing: lv,
                        planned_node_id: filesystem,
                        filesystem: FilesystemKind::Ext4,
                        label: None,
                    }),
                },
                FragmentAction {
                    id: LocalActionId::new(2),
                    operation: ProviderAction::Filesystem(FilesystemAction::Check {
                        filesystem,
                        force: true,
                    }),
                },
            ],
            dependencies: Vec::new(),
            external_requirements: vec![ExternalRequirement {
                action: LocalActionId::new(0),
                prerequisite: ActionPrerequisite::FilesystemCheckPassed(filesystem),
            }],
            origin: origin(filesystem, "create, check, and resize filesystem"),
            diagnostics: Vec::new(),
        };
        let plan = compose_plan(&current, vec![fragment.clone()])
            .unwrap_or_else(|error| panic!("bind check in created filesystem context: {error}"));
        assert_eq!(
            plan.ordered_actions()
                .iter()
                .map(|action| action.local_id.get())
                .collect::<Vec<_>>(),
            vec![1, 2, 0]
        );
        let simulation = crate::simulate_plan(&current, &plan)
            .unwrap_or_else(|error| panic!("replay new filesystem check and resize: {error}"));
        assert!(
            simulation.steps[2]
                .checks
                .iter()
                .any(|check| check.prerequisite
                    == ActionPrerequisite::FilesystemCheckPassed(filesystem)
                    && check.status
                        == crate::CheckStatus::Satisfied(crate::CheckEvidence::PredictedEffect))
        );
        assert!(simulation.steps[2].checks.iter().any(|check| matches!(&check.prerequisite, ActionPrerequisite::FilesystemMinimumSize { filesystem: checked, .. } if *checked == filesystem)
            && matches!(check.status, crate::CheckStatus::Unknown { .. })));
        assert_eq!(current, baseline);
        let mut stale = fragment.clone();
        stale.actions.push(FragmentAction {
            id: LocalActionId::new(3),
            operation: ProviderAction::Filesystem(FilesystemAction::Resize {
                filesystem,
                new_size: Bytes::new(50),
            }),
        });
        stale.external_requirements.push(ExternalRequirement {
            action: LocalActionId::new(3),
            prerequisite: ActionPrerequisite::FilesystemCheckPassed(filesystem),
        });
        stale.dependencies.push(FragmentDependency {
            before: LocalActionId::new(0),
            after: LocalActionId::new(3),
            reason: DependencyReason::RequestedOrder,
        });
        assert!(matches!(
            compose_plan(&current, vec![stale]),
            Err(CompositionError::ActionRejected { action, .. }) if action == ActionId::new(3)
        ));
        fragment.dependencies.push(FragmentDependency {
            before: LocalActionId::new(0),
            after: LocalActionId::new(2),
            reason: DependencyReason::RequestedOrder,
        });
        assert!(matches!(
            compose_plan(&current, vec![fragment]),
            Err(CompositionError::ActionRejected { .. })
        ));
    }
    /// An empty action list cannot bypass storage structure checks on in-memory snapshots.
    #[test]
    fn empty_plans_reject_invalid_storage_graphs() {
        let (mut current, vg) = current_with_vg();
        current
            .graph
            .insert_dependency(storage_core::model::Dependency {
                from: vg,
                to: vg,
                kind: storage_core::model::DependencyKind::Backs,
            });
        assert!(matches!(
            compose_plan(&current, Vec::new()),
            Err(CompositionError::Structure(_))
        ));
        let empty = ExecutionPlan::new(Vec::new(), Vec::new());
        assert!(matches!(
            crate::simulate_plan(&current, &empty),
            Err(PlanSimulationError::Structure(_))
        ));
    }
}
