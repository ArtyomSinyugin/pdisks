//! Whole-plan simulation and semantic goal comparison.

use std::{collections::HashSet, path::PathBuf};

use storage_core::model::{
    BootSpec, CurrentState, DesiredState, MountContext, MountEntry, NodeGraph, ObservedMount,
};
use storage_provider::{ActionPrerequisite, FactInvalidation, FragmentOrigin};
use thiserror::Error;

use crate::{
    ActionId, ActionSimulationError, CapacityEvidence, CapacityEvidenceError, CheckStatus,
    ExecutionPlan, PrerequisiteCheck, Projection,
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
}

impl SimulationReport {
    /// Reports complete planning evidence only; live admission is still required.
    pub fn is_complete(&self) -> bool {
        self.unresolved_obligations.is_empty()
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
    })
}

/// Failure while replaying a frozen execution plan.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanSimulationError {
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
    let mut projection =
        Projection::from_current(current).with_capacity_evidence(capacity_evidence)?;
    let mut steps = Vec::with_capacity(plan.actions().len());
    for action in plan.ordered_actions() {
        let result = projection
            .simulate_action(&action.operation)
            .map_err(|source| PlanSimulationError::Action {
                action: action.id,
                source,
            })?;
        if result.contract != action.informational_contract {
            return Err(PlanSimulationError::ContractMismatch(action.id));
        }
        steps.push(SimulatedStep {
            action: action.id,
            checks: result.checks,
            invalidated_facts: result.invalidated_facts,
        });
    }
    Ok(SimulationResult { projection, steps })
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
pub fn compare_goal(projection: &Projection, goal: &DesiredState) -> Vec<GoalCheck> {
    let mut checks = vec![GoalCheck {
        aspect: GoalAspect::Graph,
        status: if graphs_match(projection.graph(), &goal.graph) {
            GoalStatus::Satisfied
        } else {
            GoalStatus::Violated {
                reason: "projected storage graph does not match the requested graph".to_owned(),
            }
        },
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

/// Compares graph semantics while ignoring observed `NodeFacts`.
fn graphs_match(projected: &NodeGraph, requested: &NodeGraph) -> bool {
    projected.nodes().len() == requested.nodes().len()
        && projected.nodes().all(|(id, node)| {
            requested
                .node(id)
                .is_some_and(|goal| goal.kind == node.kind)
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
            .all(|relation| requested.relations().any(|candidate| candidate == relation))
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

/// Compares mount options as an order-independent set.
fn options_match(left: &[String], right: &[String]) -> bool {
    left.iter().collect::<HashSet<_>>() == right.iter().collect::<HashSet<_>>()
}

#[cfg(test)]
mod tests {
    use storage_core::model::{
        Environment, MountSource, MountSpec, MountState, NodeGraph, SystemEnvironment,
    };

    use super::*;

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
}
