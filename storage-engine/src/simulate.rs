//! Whole-plan simulation and semantic goal comparison.

use std::path::PathBuf;

use storage_core::model::{
    BootSpec, CurrentState, DesiredState, MountContext, MountEntry, NodeGraph, ObservedMount,
};
use storage_provider::{
    ActionDescriptionContext, ActionPrerequisite, FactInvalidation, FragmentOrigin,
    verify_imported_contract,
};
use thiserror::Error;

use crate::{
    ActionId, ActionSimulationError, CapacityEvidence, CapacityEvidenceError, CheckStatus,
    ExecutionPlan, PrerequisiteCheck, Projection, ProtectedScope, ProtectionViolation,
};

/// One trusted action result in plan order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedStep {
    /// Global identity of the simulated action.
    pub action: ActionId,
    /// Prerequisite results calculated immediately before the action.
    pub checks: Vec<PrerequisiteCheck>,
    /// Observation aspects made stale by this action rather than new measurements.
    pub invalidated_facts: Vec<FactInvalidation>,
}

/// Conditional projection produced without mutating the observed baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationResult {
    /// Final internal projection, including runtime-only guarantees.
    pub projection: Projection,
    /// Action-by-action prerequisite trace.
    pub steps: Vec<SimulatedStep>,
}

/// Coverage of an explicit request by typed actions, without claiming execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentCoverage {
    /// Index in the immutable requested goal.
    pub index: usize,
    /// Actions attributed to this intent, including any supporting operations.
    pub actions: Vec<ActionId>,
    /// Whether those actions implement the supported typed request.
    pub covered: bool,
}

/// Evidence missing from a successful but conditional simulation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimulationObligation {
    /// Condition that must hold before a particular action.
    Prerequisite {
        /// Action requiring the evidence at its own predecessor state.
        action: ActionId,
        /// Exact trusted condition to check.
        prerequisite: ActionPrerequisite,
        /// Missing evidence, rather than permission to assume success.
        required_evidence: String,
    },
    /// Requested final aspect that pure simulation cannot establish.
    Goal {
        /// Goal aspect requiring evidence.
        aspect: GoalAspect,
        /// Missing evidence for this requested result.
        required_evidence: String,
    },
}

/// Goal-aware analysis; a prediction never grants live execution permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationReport {
    /// Conditional projection and the ordered prerequisite trace.
    pub simulation: SimulationResult,
    /// Three-valued comparisons against the requested final state.
    pub goal_checks: Vec<GoalCheck>,
    /// Typed action coverage of explicit requests, not actual operation results.
    pub intent_coverage: Vec<IntentCoverage>,
    /// All unknown conditions, retained even if the final graph matches.
    pub unresolved_obligations: Vec<SimulationObligation>,
    /// Planned changes intersecting an explicitly protected node or mount.
    pub protection_violations: Vec<ProtectionViolation>,
}

impl SimulationReport {
    /// Reports complete planning evidence only; live admission is still required.
    pub fn is_complete(&self) -> bool {
        self.protection_violations.is_empty()
            && self.unresolved_obligations.is_empty()
            && self
                .goal_checks
                .iter()
                .all(|check| check.status == GoalStatus::Satisfied)
            && self.intent_coverage.iter().all(|coverage| coverage.covered)
    }
}

/// Replays a plan and reports goal coverage without extra provider evidence.
pub fn simulate_plan_for_goal(
    current: &CurrentState,
    plan: &ExecutionPlan,
    goal: &DesiredState,
) -> Result<SimulationReport, PlanSimulationError> {
    simulate_plan_for_goal_with_evidence(current, plan, goal, &[])
}

/// Replays with baseline capacity evidence and preserves all unknown obligations.
pub fn simulate_plan_for_goal_with_evidence(
    current: &CurrentState,
    plan: &ExecutionPlan,
    goal: &DesiredState,
    capacity_evidence: &[CapacityEvidence],
) -> Result<SimulationReport, PlanSimulationError> {
    simulate_plan_for_goal_with_policy(
        current,
        plan,
        goal,
        capacity_evidence,
        &ProtectedScope::default(),
    )
}

/// Replays a plan against the goal and an explicit protected area, retaining every conflict.
/// An empty policy protects no area; it does not authorize any real system write.
pub fn simulate_plan_for_goal_with_policy(
    current: &CurrentState,
    plan: &ExecutionPlan,
    goal: &DesiredState,
    capacity_evidence: &[CapacityEvidence],
    protected: &ProtectedScope,
) -> Result<SimulationReport, PlanSimulationError> {
    goal.graph.validate_structure()?;
    crate::validate::validate_protected_scope(protected)?;
    for action in plan.actions() {
        if let FragmentOrigin::Intent { index } = action.origin
            && index >= goal.intents.len()
        {
            return Err(PlanSimulationError::UnknownIntentOrigin {
                action: action.id,
                index,
            });
        }
    }
    let simulation = simulate_plan_with_evidence(current, plan, capacity_evidence)?;
    let protection_violations =
        crate::validate::check_protected_scope(plan, &simulation, protected);
    let goal_checks = compare_goal(&simulation.projection, goal);
    let ordered = plan.ordered_actions();
    let intent_coverage = goal
        .intents
        .iter()
        .enumerate()
        .map(|(index, intent)| {
            let actions: Vec<_> = ordered
                .iter()
                .copied()
                .filter(|action| action.origin == FragmentOrigin::Intent { index })
                .collect();
            IntentCoverage {
                index,
                covered: crate::compose::actions_cover_intent(
                    intent,
                    &actions
                        .iter()
                        .map(|action| &action.operation)
                        .collect::<Vec<_>>(),
                ),
                actions: actions.iter().map(|action| action.id).collect(),
            }
        })
        .collect();
    let mut unresolved_obligations = Vec::new();
    for step in &simulation.steps {
        for check in &step.checks {
            if let CheckStatus::Unknown { required_evidence } = &check.status {
                unresolved_obligations.push(SimulationObligation::Prerequisite {
                    action: step.action,
                    prerequisite: check.prerequisite.clone(),
                    required_evidence: required_evidence.clone(),
                });
            }
        }
    }
    for check in &goal_checks {
        if let GoalStatus::Unknown { required_evidence } = &check.status {
            unresolved_obligations.push(SimulationObligation::Goal {
                aspect: check.aspect.clone(),
                required_evidence: required_evidence.clone(),
            });
        }
    }
    Ok(SimulationReport {
        simulation,
        goal_checks,
        intent_coverage,
        unresolved_obligations,
        protection_violations,
    })
}

/// Failure while replaying a frozen execution plan.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanSimulationError {
    /// The supplied storage graph has an invalid representation.
    #[error(transparent)]
    Structure(#[from] storage_core::model::GraphError),
    /// A protection policy contains a relative, traversing, or NUL-bearing mount path.
    #[error("invalid protected mount target or context: {0:?}")]
    InvalidProtectedMount(crate::ProtectedMount),
    /// Plan provenance references an intent absent from the supplied goal.
    #[error("action {action:?} references absent goal intent {index}")]
    UnknownIntentOrigin {
        /// Action whose provenance is invalid for this goal.
        action: ActionId,
        /// Missing intent index.
        index: usize,
    },
    /// Provider capacity evidence contains duplicate domains.
    #[error(transparent)]
    CapacityEvidence(#[from] CapacityEvidenceError),
    /// One action could not be projected in the frozen plan order.
    #[error("action {action:?} failed during plan simulation: {source}")]
    Action {
        /// Action that failed.
        action: ActionId,
        /// Trusted prerequisite or effect failure.
        source: ActionSimulationError,
    },
    /// Recalculation no longer matches the informational contract stored in the plan.
    #[error("action {0:?} informational contract does not match trusted recalculation")]
    ContractMismatch(ActionId),
}

/// Simulates a complete plan without extra provider evidence.
pub fn simulate_plan(
    current: &CurrentState,
    plan: &ExecutionPlan,
) -> Result<SimulationResult, PlanSimulationError> {
    simulate_plan_with_evidence(current, plan, &[])
}

/// Simulates a complete plan with baseline provider capacity evidence.
pub fn simulate_plan_with_evidence(
    current: &CurrentState,
    plan: &ExecutionPlan,
    capacity_evidence: &[CapacityEvidence],
) -> Result<SimulationResult, PlanSimulationError> {
    replay_plan(current, plan, capacity_evidence).map(|(_, simulation)| simulation)
}

/// Replays through the shared interpreter and retains only recalculated trusted contracts.
pub(crate) fn replay_plan(
    current: &CurrentState,
    plan: &ExecutionPlan,
    capacity_evidence: &[CapacityEvidence],
) -> Result<(ExecutionPlan, SimulationResult), PlanSimulationError> {
    current.graph.validate_structure()?;
    let mut projection =
        Projection::from_current(current).with_capacity_evidence(capacity_evidence)?;
    let mut trusted_actions = Vec::with_capacity(plan.actions().len());
    let mut steps = Vec::with_capacity(plan.actions().len());
    for action in plan.ordered_actions() {
        verify_imported_contract(
            &action.operation,
            action.informational_contract.clone(),
            ActionDescriptionContext {
                graph: projection.graph(),
            },
        )
        .map_err(|_| PlanSimulationError::ContractMismatch(action.id))?;
        let result = projection
            .simulate_action(&action.operation)
            .map_err(|source| PlanSimulationError::Action {
                action: action.id,
                source,
            })?;
        let mut trusted_action = action.clone();
        trusted_action.informational_contract = result.contract;
        trusted_actions.push(trusted_action);
        steps.push(SimulatedStep {
            action: action.id,
            checks: result.checks,
            invalidated_facts: result.invalidated_facts,
        });
    }
    trusted_actions.sort_by_key(|action| action.id);
    Ok((
        ExecutionPlan::new(trusted_actions, plan.dependencies().to_vec()),
        SimulationResult { projection, steps },
    ))
}

/// Requested aspect evaluated against a final projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalAspect {
    /// Complete semantic storage graph.
    Graph,
    /// One requested runtime mount.
    RuntimeMount {
        /// Requested target path.
        target: PathBuf,
        /// Namespace containing the target.
        context: MountContext,
    },
    /// One requested persistent mount entry.
    PersistentMount {
        /// Requested target path.
        target: PathBuf,
        /// Configuration context containing the entry.
        context: MountContext,
    },
    /// Requested boot configuration.
    Boot,
}

/// Three-valued result of comparing one goal aspect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalStatus {
    /// The final projection establishes the requested value.
    Satisfied,
    /// The final projection contradicts the requested value.
    Violated {
        /// Explanation of the known mismatch.
        reason: String,
    },
    /// The model lacks evidence needed to prove or disprove the request.
    Unknown {
        /// Evidence required from a later planning or execution stage.
        required_evidence: String,
    },
}

/// Goal-comparison result tied to one requested aspect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalCheck {
    /// Requested aspect being checked.
    pub aspect: GoalAspect,
    /// Comparison result.
    pub status: GoalStatus,
}

/// Compares the final projection with the requested semantic goal.
/// Requested `NodeSpec.size == None` leaves only capacity unmanaged; kinds and topology are exact.
/// An unknown projected capacity against an explicit requested size remains `Unknown`.
pub fn compare_goal(projection: &Projection, goal: &DesiredState) -> Vec<GoalCheck> {
    let mut checks = vec![GoalCheck {
        aspect: GoalAspect::Graph,
        status: compare_graph(projection.graph(), &goal.graph),
    }];
    for entry in &goal.mounts.entries {
        checks.push(runtime_mount_check(projection.runtime_mounts(), entry));
        if entry.persistent {
            checks.push(persistent_mount_check(projection, entry));
        }
    }
    checks.push(GoalCheck {
        aspect: GoalAspect::Boot,
        status: match goal.boot {
            BootSpec::Unmanaged => GoalStatus::Satisfied,
            BootSpec::Uefi(_) | BootSpec::Bios(_) => GoalStatus::Unknown {
                required_evidence: "boot planning and verification".to_owned(),
            },
        },
    });
    checks
}

/// Compares exact topology/kinds and the specifically managed capacity field.
/// Only requested `NodeSpec.size == None` leaves capacity unconstrained.
fn compare_graph(projected: &NodeGraph, requested: &NodeGraph) -> GoalStatus {
    let topology_matches = projected.nodes().len() == requested.nodes().len()
        && projected.nodes().all(|(id, node)| {
            requested
                .node(id)
                .is_some_and(|goal| goal.kind.kind == node.kind.kind)
        })
        && projected.dependencies().len() == requested.dependencies().len()
        && projected.dependencies().all(|dependency| {
            requested
                .dependencies()
                .any(|candidate| candidate == dependency)
        })
        && projected.relations().len() == requested.relations().len()
        && projected
            .relations()
            .all(|relation| requested.relations().any(|candidate| candidate == relation));
    if !topology_matches {
        return GoalStatus::Violated {
            reason:
                "projected storage topology or node properties do not match the requested graph"
                    .to_owned(),
        };
    }
    let mut unknown = false;
    for (id, goal) in requested.nodes() {
        if let Some(required) = goal.kind.size {
            match projected.node(id).and_then(|node| node.kind.size) {
                Some(actual) if actual == required => {}
                Some(_) => {
                    return GoalStatus::Violated {
                        reason: "projected capacity does not match the requested size".to_owned(),
                    };
                }
                None => unknown = true,
            }
        }
    }
    if unknown {
        GoalStatus::Unknown {
            required_evidence:
                "provider capacity evidence for nodes with explicitly requested sizes".to_owned(),
        }
    } else {
        GoalStatus::Satisfied
    }
}

/// Checks one requested runtime mount relation.
fn runtime_mount_check(mounts: &[ObservedMount], entry: &MountEntry) -> GoalCheck {
    let present = mounts.iter().any(|mount| {
        mount.source == entry.source
            && mount.target == entry.target
            && mount.context == entry.context
            && options_match(&mount.options, &entry.options)
    });
    GoalCheck {
        aspect: GoalAspect::RuntimeMount {
            target: entry.target.clone(),
            context: entry.context.clone(),
        },
        status: if present {
            GoalStatus::Satisfied
        } else {
            GoalStatus::Violated {
                reason: "requested runtime mount is absent from the projection".to_owned(),
            }
        },
    }
}

/// Checks a persistent entry without treating an unread baseline as empty.
fn persistent_mount_check(projection: &Projection, requested: &MountEntry) -> GoalCheck {
    let present = projection.persistent_mounts().iter().any(|entry| {
        entry.source == requested.source
            && entry.target == requested.target
            && entry.context == requested.context
            && options_match(&entry.options, &requested.options)
    });
    GoalCheck {
        aspect: GoalAspect::PersistentMount {
            target: requested.target.clone(),
            context: requested.context.clone(),
        },
        status: if present {
            GoalStatus::Satisfied
        } else if projection.persistent_mount_was_projected(&requested.target, &requested.context) {
            GoalStatus::Violated {
                reason: "planned persistent mount entry does not match the request".to_owned(),
            }
        } else {
            GoalStatus::Unknown {
                required_evidence: "persistent mount configuration baseline or planned effect"
                    .to_owned(),
            }
        },
    }
}

/// Compares opaque option sequences exactly because later options can override earlier ones.
fn options_match(left: &[String], right: &[String]) -> bool {
    left == right
}

#[cfg(test)]
mod tests {
    use storage_core::model::{
        Environment, MountSource, MountSpec, MountState, NodeGraph, SystemEnvironment,
    };

    use super::*;

    /// An empty plan preserves observations without inventing persistent configuration.
    #[test]
    fn empty_plan_preserves_observed_configuration() {
        let current = CurrentState {
            graph: NodeGraph::new(),
            mounts: MountState {
                entries: vec![ObservedMount {
                    source: MountSource::Tmpfs,
                    target: PathBuf::from("/run"),
                    options: vec!["nosuid".to_owned()],
                    context: MountContext::Host,
                }],
            },
            environment: Environment {
                host: SystemEnvironment::default(),
                target: None,
            },
            diagnostics: Vec::new(),
        };
        let before = current.clone();
        let plan = crate::compose_plan(&current, Vec::new())
            .unwrap_or_else(|error| panic!("compose empty plan: {error}"));
        let result = simulate_plan(&current, &plan)
            .unwrap_or_else(|error| panic!("simulate empty plan: {error}"));
        assert_eq!(result.projection.graph(), &current.graph);
        assert_eq!(result.projection.runtime_mounts(), current.mounts.entries);
        assert!(result.projection.persistent_mounts().is_empty());
        assert!(result.steps.is_empty());
        assert_eq!(current, before);
    }

    /// Conflicting option order and repeated overrides must not prove the requested mount goal.
    #[test]
    fn mount_goals_preserve_option_order_and_multiplicity() {
        for actual in [vec!["rw", "ro"], vec!["ro", "rw", "ro"]] {
            let entry = MountEntry {
                source: MountSource::Tmpfs,
                target: PathBuf::from("/data"),
                options: vec!["ro".to_owned(), "rw".to_owned()],
                persistent: true,
                context: MountContext::Host,
            };
            let mut actual_entry = entry.clone();
            actual_entry.options = actual.into_iter().map(str::to_owned).collect();
            let current = CurrentState {
                graph: NodeGraph::new(),
                mounts: MountState {
                    entries: vec![ObservedMount {
                        source: actual_entry.source.clone(),
                        target: actual_entry.target.clone(),
                        options: actual_entry.options.clone(),
                        context: actual_entry.context.clone(),
                    }],
                },
                environment: Environment {
                    host: SystemEnvironment::default(),
                    target: None,
                },
                diagnostics: Vec::new(),
            };
            let mut projection = Projection::from_current(&current);
            projection
                .apply_effect(&storage_provider::PredictedEffect {
                    graph: Vec::new(),
                    mounts: vec![storage_provider::MountEffect::SetPersistent(
                        actual_entry.clone(),
                    )],
                    allocations: Vec::new(),
                })
                .unwrap_or_else(|error| panic!("project persistent mount: {error}"));
            assert!(matches!(
                runtime_mount_check(projection.runtime_mounts(), &entry).status,
                GoalStatus::Violated { .. }
            ));
            assert!(matches!(
                persistent_mount_check(&projection, &entry).status,
                GoalStatus::Violated { .. }
            ));
            assert_eq!(
                runtime_mount_check(projection.runtime_mounts(), &actual_entry).status,
                GoalStatus::Satisfied
            );
            assert_eq!(
                persistent_mount_check(&projection, &actual_entry).status,
                GoalStatus::Satisfied
            );
        }
    }

    /// Runtime evidence does not fabricate an unread persistent configuration.
    #[test]
    fn persistent_mount_goal_remains_unknown_without_persistent_evidence() {
        let entry = MountEntry {
            source: MountSource::Tmpfs,
            target: PathBuf::from("/data"),
            options: vec!["rw".to_owned()],
            persistent: true,
            context: MountContext::Host,
        };
        let current = CurrentState {
            graph: NodeGraph::new(),
            mounts: MountState {
                entries: vec![ObservedMount {
                    source: entry.source.clone(),
                    target: entry.target.clone(),
                    options: entry.options.clone(),
                    context: entry.context.clone(),
                }],
            },
            environment: Environment {
                host: SystemEnvironment::default(),
                target: None,
            },
            diagnostics: Vec::new(),
        };
        let goal = DesiredState {
            graph: NodeGraph::new(),
            mounts: MountSpec {
                entries: vec![entry],
            },
            boot: BootSpec::Unmanaged,
            intents: Vec::new(),
        };

        let plan = crate::compose_plan_for_goal(&current, &goal, Vec::new())
            .unwrap_or_else(|error| panic!("compose conditional persistent goal: {error}"));
        let report = simulate_plan_for_goal(&current, &plan, &goal)
            .unwrap_or_else(|error| panic!("report conditional persistent goal: {error}"));
        assert_eq!(report.unresolved_obligations.len(), 1);
        assert!(matches!(
            &report.unresolved_obligations[0],
            SimulationObligation::Goal {
                aspect: GoalAspect::PersistentMount { .. },
                ..
            }
        ));
        assert!(!report.is_complete());
        let checks = report.goal_checks;

        assert!(checks.iter().any(|check| matches!(
            check,
            GoalCheck {
                aspect: GoalAspect::RuntimeMount { .. },
                status: GoalStatus::Satisfied,
            }
        )));
        assert!(checks.iter().any(|check| matches!(
            check,
            GoalCheck {
                aspect: GoalAspect::PersistentMount { .. },
                status: GoalStatus::Unknown { .. },
            }
        )));
    }

    /// A matching graph cannot cover an explicit operation, and mismatches remain visible.
    #[test]
    fn report_distinguishes_complete_uncovered_and_violated_goals() {
        let current = CurrentState {
            graph: NodeGraph::new(),
            mounts: MountState::default(),
            environment: Environment {
                host: SystemEnvironment::default(),
                target: None,
            },
            diagnostics: Vec::new(),
        };
        let mut goal = DesiredState {
            graph: current.graph.clone(),
            mounts: MountSpec::default(),
            boot: BootSpec::Unmanaged,
            intents: Vec::new(),
        };
        let plan = crate::compose_plan(&current, Vec::new())
            .unwrap_or_else(|error| panic!("compose empty plan: {error}"));
        assert!(
            simulate_plan_for_goal(&current, &plan, &goal)
                .unwrap_or_else(|error| panic!("report complete goal: {error}"))
                .is_complete()
        );
        goal.intents
            .push(storage_core::model::OperationIntent::Check {
                target: storage_core::model::NodeId::new(),
                force: true,
            });
        let report = simulate_plan_for_goal(&current, &plan, &goal)
            .unwrap_or_else(|error| panic!("report uncovered goal: {error}"));
        assert!(!report.is_complete());
        assert!(!report.intent_coverage[0].covered);
        assert!(report.intent_coverage[0].actions.is_empty());
        goal.intents.clear();
        goal.mounts.entries.push(MountEntry {
            source: MountSource::Tmpfs,
            target: PathBuf::from("/missing"),
            options: Vec::new(),
            persistent: false,
            context: MountContext::Host,
        });
        let report = simulate_plan_for_goal(&current, &plan, &goal)
            .unwrap_or_else(|error| panic!("report violated goal: {error}"));
        assert!(!report.is_complete());
        assert!(report.unresolved_obligations.is_empty());
        assert!(
            report
                .goal_checks
                .iter()
                .any(|check| matches!(check.status, GoalStatus::Violated { .. }))
        );
    }
    /// Capacity has an explicit unmanaged rule; unknown values never conceal other mismatches.
    #[test]
    fn graph_goal_distinguishes_unmanaged_unknown_and_wrong_capacity() {
        use storage_core::model::{Bytes, Node, NodeFacts, NodeId, NodeKind, NodeSpec};
        let id = NodeId::new();
        let mut projected = NodeGraph::new();
        projected
            .insert_new(
                id,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let mut requested = projected.clone();
        assert_eq!(compare_graph(&projected, &requested), GoalStatus::Satisfied);
        requested
            .replace_existing(
                id,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: Some(Bytes::new(100)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        assert!(matches!(
            compare_graph(&projected, &requested),
            GoalStatus::Unknown { .. }
        ));
        projected
            .replace_existing(
                id,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: Some(Bytes::new(90)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        assert!(matches!(
            compare_graph(&projected, &requested),
            GoalStatus::Violated { .. }
        ));
        requested
            .replace_existing(
                id,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        assert_eq!(compare_graph(&projected, &requested), GoalStatus::Satisfied);
        requested
            .replace_existing(
                id,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Loop { backing_file: None },
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        assert!(matches!(
            compare_graph(&projected, &requested),
            GoalStatus::Violated { .. }
        ));
        // An unknown node does not hide a different node with a known wrong size.
        let unknown = NodeId::new();
        projected
            .insert_new(
                unknown,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        requested
            .insert_new(
                unknown,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: Some(Bytes::new(100)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        requested
            .replace_existing(
                id,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: Some(Bytes::new(100)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        assert!(matches!(
            compare_graph(&projected, &requested),
            GoalStatus::Violated { .. }
        ));
    }
}
