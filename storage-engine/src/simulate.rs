//! Whole-plan simulation and semantic goal comparison.

use std::{collections::HashSet, path::PathBuf};

use storage_core::model::{
    BootSpec, CurrentState, DesiredState, MountContext, MountEntry, NodeGraph, ObservedMount,
};
use storage_provider::FactInvalidation;
use thiserror::Error;

use crate::{
    ActionId, ActionSimulationError, CapacityEvidence, CapacityEvidenceError, ExecutionPlan,
    PrerequisiteCheck, Projection,
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

/// Failure while replaying a frozen execution plan.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanSimulationError {
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

        let checks = compare_goal(&Projection::from_current(&current), &goal);

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
}
