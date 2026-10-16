//! Conservative result checks over one retained actual post-backend observation.

use serde::Serialize;
use storage_core::model::{MountContext, MountSource, NodeKind};
use storage_provider::Postcondition;

use super::{ObservationError, correlated_node, qualify_observed_status, unknown};
use crate::{
    HistoricalBaseline, LiveCheck, LiveCheckStatus, LiveObservation, ObservationReference,
};

/// Observed postconditions retain frozen identities and the exact publication they inspect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservedPostconditions {
    /// Publication to bind to the runtime's retained completion, never a later substitute.
    pub observation: ObservationReference,
    /// Exactly the requested postconditions in their original order.
    pub checks: Vec<LiveCheck<Postcondition>>,
}

/// Checks supported existing-object results without applying a predicted effect.
/// The trusted host must supply the actual post-backend publication; this does not prove dispatch.
/// Planned outputs, absent coverage, and provider-owned check results remain unknown.
pub fn check_observed_postconditions(
    baseline: &HistoricalBaseline,
    observation: &LiveObservation,
    postconditions: &[Postcondition],
) -> Result<ObservedPostconditions, ObservationError> {
    observation.validate()?;
    baseline.snapshot.graph.validate_structure()?;
    Ok(ObservedPostconditions {
        observation: observation.reference,
        checks: postconditions
            .iter()
            .map(|condition| LiveCheck {
                condition: condition.clone(),
                status: qualify_observed_status(
                    observation,
                    check_result(baseline, observation, condition),
                ),
            })
            .collect(),
    })
}

/// Correlates existing objects before inspecting actual properties; missing matches are not success.
fn check_result(
    baseline: &HistoricalBaseline,
    observation: &LiveObservation,
    condition: &Postcondition,
) -> LiveCheckStatus {
    match condition {
        Postcondition::DependencyPresent(edge) | Postcondition::DependencyAbsent(edge) => {
            let (from, _) = match correlated_node(baseline, observation, edge.from) {
                Ok(value) => value,
                Err(status) => return status,
            };
            let (to, _) = match correlated_node(baseline, observation, edge.to) {
                Ok(value) => value,
                Err(status) => return status,
            };
            let present =
                observation.snapshot.graph.dependencies().any(|actual| {
                    actual.from == from && actual.to == to && actual.kind == edge.kind
                });
            return match (condition, present) {
                (Postcondition::DependencyPresent(_), true) => LiveCheckStatus::Satisfied,
                (Postcondition::DependencyAbsent(_), true) => {
                    violated("dependency is still present in the observed graph")
                }
                _ => unknown("complete dependency coverage is required to establish absence"),
            };
        }
        Postcondition::RuntimeMountPresent {
            source: MountSource::Filesystem(source),
            target,
            context: MountContext::Host,
        } => {
            let (filesystem, node) = match correlated_node(baseline, observation, *source) {
                Ok(value) => value,
                Err(status) => return status,
            };
            if !matches!(node.kind.kind, NodeKind::Filesystem { .. }) {
                return violated("correlated mount source is not a filesystem");
            }
            return if observation.snapshot.mounts.entries.iter().any(|mount| {
                mount.source == MountSource::Filesystem(filesystem)
                    && mount.target == *target
                    && mount.context == MountContext::Host
            }) {
                LiveCheckStatus::Satisfied
            } else {
                unknown(
                    "the exact filesystem mount was not observed; complete namespace coverage is required",
                )
            };
        }
        Postcondition::RuntimeMountAbsent { target, context } => {
            return if observation
                .snapshot
                .mounts
                .entries
                .iter()
                .any(|mount| mount.target == *target && mount.context == *context)
            {
                violated("mount target is still occupied in an observed context")
            } else {
                unknown("complete mount namespace coverage is required to establish absence")
            };
        }
        _ => {}
    }
    let source = match condition {
        Postcondition::NodeMatches { id, .. } => *id,
        Postcondition::NodeSize { node, .. } | Postcondition::NodeReprobed(node) => *node,
        Postcondition::FilesystemLabel { filesystem, .. } => *filesystem,
        Postcondition::NodeAbsent(node) => *node,
        _ => {
            return unknown(
                "provider result or complete observation coverage is required for this postcondition",
            );
        }
    };
    let (_, observed) = match correlated_node(baseline, observation, source) {
        Ok(value) => value,
        Err(status) => return status,
    };
    match condition {
        Postcondition::NodeAbsent(_) => {
            violated("correlated object is still present in the observation")
        }
        Postcondition::NodeReprobed(_) => LiveCheckStatus::Satisfied,
        Postcondition::NodeSize { size, .. } => match observed.kind.size {
            Some(actual) if actual == *size => LiveCheckStatus::Satisfied,
            Some(_) => violated("observed size differs from the requested result"),
            None => unknown("fresh observed node size is required"),
        },
        Postcondition::NodeMatches { spec, .. } => {
            if observed.kind.kind != spec.kind {
                return violated(
                    "observed node kind or properties differ from the requested result",
                );
            }
            match (spec.size, observed.kind.size) {
                (None, _) => LiveCheckStatus::Satisfied,
                (Some(expected), Some(actual)) if expected == actual => LiveCheckStatus::Satisfied,
                (Some(_), Some(_)) => violated("observed size differs from the requested result"),
                (Some(_), None) => unknown("fresh observed node size is required"),
            }
        }
        Postcondition::FilesystemLabel { label, .. } => match &observed.kind.kind {
            NodeKind::Filesystem { label: actual, .. } if actual == label => {
                LiveCheckStatus::Satisfied
            }
            NodeKind::Filesystem { .. } => {
                violated("observed filesystem label differs from the requested result")
            }
            _ => violated("correlated object is not a filesystem"),
        },
        _ => unknown("unsupported observed node result"),
    }
}

/// Reports a concrete mismatch separately from an evidence gap.
fn violated(reason: &str) -> LiveCheckStatus {
    LiveCheckStatus::Violated {
        reason: reason.into(),
    }
}
