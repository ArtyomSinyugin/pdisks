//! Actual final-state comparison; neither predicted graphs nor backend success establish these facts.

use super::{ObservationError, qualify_observed_status, unknown, verify::result_node};
use crate::{
    ExecutionContext, GoalAspect, LiveCheck, LiveCheckStatus, LiveObservation,
    editing::NodeCorrelation,
};
use std::collections::{HashMap, HashSet};
use storage_core::model::{BootSpec, NodeId, OperationIntent};
use storage_provider::{LvmAction, ProviderAction};

/// Checks every requested aspect using one actual publication and runtime-verified output identities.
/// Mount configuration and managed boot remain unknown until their independent evidence adapters exist.
/// Scope completeness and protected contents must still be established by the runtime host.
pub fn check_observed_goal(
    context: &ExecutionContext<'_>,
    observed: &LiveObservation,
    bindings: &[NodeCorrelation],
) -> Result<Vec<LiveCheck<GoalAspect>>, ObservationError> {
    observed.validate()?;
    context
        .historical_baseline
        .snapshot
        .graph
        .validate_structure()?;
    context.requested_goal.graph.validate_structure()?;
    let mut checks = vec![LiveCheck {
        condition: GoalAspect::Graph,
        status: qualify_observed_status(observed, graph_status(context, observed, bindings)),
    }];
    for mount in &context.requested_goal.mounts.runtime {
        checks.push(LiveCheck {
            condition: GoalAspect::RuntimeMount {
                target: mount.target.clone(),
                context: mount.context.clone(),
            },
            status: unknown("independent runtime mount goal verification is required"),
        });
    }
    for mount in &context.requested_goal.mounts.persistent {
        checks.push(LiveCheck {
            condition: GoalAspect::PersistentMount {
                target: mount.target.clone(),
                context: mount.context.clone(),
            },
            status: unknown("independent persistent configuration verification is required"),
        });
    }
    checks.push(LiveCheck {
        condition: GoalAspect::Boot,
        status: match context.requested_goal.boot {
            BootSpec::Unmanaged => LiveCheckStatus::Satisfied,
            _ => unknown("independent managed boot verification is required"),
        },
    });
    Ok(checks)
}

/// Compares exact graph topology and managed properties after one-to-one stable correlation.
fn graph_status(
    context: &ExecutionContext<'_>,
    observed: &LiveObservation,
    bindings: &[NodeCorrelation],
) -> LiveCheckStatus {
    let goal = &context.requested_goal.graph;
    let actual = &observed.snapshot.graph;
    let mut mapping = HashMap::new();
    for (id, requested) in goal.nodes() {
        let (target, node) = match result_node(context.historical_baseline, observed, *id, bindings)
        {
            Ok(value) => value,
            Err(status) => return status,
        };
        if mapping.values().any(|other| *other == target) {
            return unknown("final graph correlation is not one-to-one");
        }
        if requested.kind.kind != node.kind.kind {
            return violated("final node properties differ from the requested goal");
        }
        if let Some(size) = requested.kind.size {
            match node.kind.size {
                Some(actual) if actual == size => {}
                Some(_) => return violated("final node capacity differs from the requested goal"),
                None => return unknown("final managed node capacity is unknown"),
            }
        }
        mapping.insert(*id, target);
    }
    if goal.nodes().len() != actual.nodes().len()
        || goal.dependencies().len() != actual.dependencies().len()
        || goal.relations().len() != actual.relations().len()
        || goal.dependencies().any(|edge| {
            !actual.dependencies().any(|other| {
                other.from == mapping[&edge.from]
                    && other.to == mapping[&edge.to]
                    && other.kind == edge.kind
            })
        })
        || goal.relations().any(|edge| {
            !actual.relations().any(|other| {
                other.from == mapping[&edge.from]
                    && other.to == mapping[&edge.to]
                    && other.kind == edge.kind
            })
        })
    {
        return violated("final topology differs from the requested goal");
    }
    // Later requests may supersede earlier activation states; only the final request per node is managed.
    let mut activation = HashSet::new();
    for action in context.plan.ordered_actions().into_iter().rev() {
        if let ProviderAction::Lvm(LvmAction::SetActivation { target, active }) = &action.operation
            && activation.insert(*target)
        {
            let status = activation_status(context, observed, bindings, *target, *active);
            if status != LiveCheckStatus::Satisfied {
                return status;
            }
        }
    }
    for intent in &context.requested_goal.intents {
        match intent {
            OperationIntent::SetActivation { target, active } => {
                let status = activation_status(context, observed, bindings, *target, *active);
                if status != LiveCheckStatus::Satisfied {
                    return status;
                }
            }
            _ => {
                return unknown(
                    "independent final evidence for the requested operation intent is required",
                );
            }
        }
    }
    LiveCheckStatus::Satisfied
}

/// Rechecks final LV activation and the writable interface promised by the initial active-LV recipe.
fn activation_status(
    context: &ExecutionContext<'_>,
    observed: &LiveObservation,
    bindings: &[NodeCorrelation],
    target: NodeId,
    active: bool,
) -> LiveCheckStatus {
    let (_, node) = match result_node(context.historical_baseline, observed, target, bindings) {
        Ok(value) => value,
        Err(status) => return status,
    };
    if !matches!(node.kind.kind, storage_core::model::NodeKind::LvmLv(_)) {
        return unknown("final activation verification supports only logical volumes");
    }
    match node.size.lvm_active {
        Some(actual) if actual != active => {
            return violated("final LV activation differs from the reviewed request");
        }
        None => return unknown("final LV activation state is unknown"),
        _ => {}
    }
    if active
        && !node.size.block.as_ref().is_some_and(|block| {
            block.read_only == Some(false) && block.geometry.is_some() && !block.paths.is_empty()
        })
    {
        return unknown("final active LV requires a known writable block interface");
    }
    LiveCheckStatus::Satisfied
}

/// Keeps observed contradictions distinct from unavailable final evidence.
fn violated(reason: &str) -> LiveCheckStatus {
    LiveCheckStatus::Violated {
        reason: reason.into(),
    }
}
