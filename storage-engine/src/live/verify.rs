//! Conservative result checks over one retained actual post-backend observation.

use serde::Serialize;
use storage_core::model::NodeKind;
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
    let source = match condition {
        Postcondition::NodeMatches { id, .. } => *id,
        Postcondition::NodeSize { node, .. } | Postcondition::NodeReprobed(node) => *node,
        Postcondition::FilesystemLabel { filesystem, .. } => *filesystem,
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
