//! Checks over actual observations; these do not grant authorization or acquire resources.

use serde::Serialize;
use storage_core::model::{DiagnosticSeverity, GraphError, MountSource, Node, NodeId};
use storage_provider::{ActionPrerequisite, NodeKindClass};
use thiserror::Error;

use crate::{
    HistoricalBaseline, LiveCheck, LiveCheckStatus, LiveObservation, ObservationReference,
    editing::correlate_nodes, prerequisites::matches_kind,
};

/// Partial prerequisite evidence bound to one publication, without applicability or lease claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservedPrerequisites {
    /// Publication actually used for all checks below.
    pub observation: ObservationReference,
    /// Exactly the supplied conditions, in their original frozen identity space and order.
    pub checks: Vec<LiveCheck<ActionPrerequisite>>,
}

/// Checks observable facts after conservative UUID correlation with the historical baseline.
/// The host must supply an actual fresh probe; JSON or a simulation cannot establish provenance.
/// Unsupported facts, planned outputs, and failed correlation remain unknown, preventing dispatch.
pub fn check_observed_prerequisites(
    baseline: &HistoricalBaseline,
    observation: &LiveObservation,
    prerequisites: &[ActionPrerequisite],
) -> Result<ObservedPrerequisites, ObservationError> {
    observation.validate()?;
    baseline.snapshot.graph.validate_structure()?;
    Ok(ObservedPrerequisites {
        observation: observation.reference,
        checks: prerequisites
            .iter()
            .map(|condition| LiveCheck {
                condition: condition.clone(),
                status: qualify_observed_status(
                    observation,
                    check_node_fact(baseline, observation, condition),
                ),
            })
            .collect(),
    })
}

/// Partial or conflicting provider results cannot establish positive evidence; contradictions survive.
fn qualify_observed_status(
    observation: &LiveObservation,
    status: LiveCheckStatus,
) -> LiveCheckStatus {
    if status == LiveCheckStatus::Satisfied
        && observation
            .snapshot
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity != DiagnosticSeverity::Warning)
    {
        unknown("resolve blocking probe diagnostics before relying on observed facts")
    } else {
        status
    }
}

/// Resolves one existing node without trusting transient IDs, paths, or display names.
fn correlated_node<'a>(
    baseline: &HistoricalBaseline,
    observation: &'a LiveObservation,
    source: NodeId,
) -> Result<(NodeId, &'a Node), LiveCheckStatus> {
    let mappings = correlate_nodes(&baseline.snapshot, &observation.snapshot, &[source])
        .map_err(|error| unknown(format!("fresh identity correlation: {error}")))?;
    let target = mappings
        .first()
        .ok_or_else(|| unknown("missing identity correlation"))?
        .target;
    let node = observation
        .snapshot
        .graph
        .node(&target)
        .ok_or_else(|| unknown("correlated node is absent"))?;
    Ok((target, node))
}

/// Evaluates only direct observed facts; external authority and missing evidence stay unknown.
fn check_node_fact(
    baseline: &HistoricalBaseline,
    observation: &LiveObservation,
    condition: &ActionPrerequisite,
) -> LiveCheckStatus {
    let source = match condition {
        ActionPrerequisite::NodeExists(node)
        | ActionPrerequisite::NodeKind { node, .. }
        | ActionPrerequisite::Writable(node)
        | ActionPrerequisite::NoUnexpectedConsumers(node)
        | ActionPrerequisite::NoUnexpectedRelations(node)
        | ActionPrerequisite::FilesystemUnmounted(node) => *node,
        _ => return unknown("runtime provider evidence is required for this condition"),
    };
    let (target, node) = match correlated_node(baseline, observation, source) {
        Ok(value) => value,
        Err(status) => return status,
    };
    match condition {
        ActionPrerequisite::NodeExists(_) => LiveCheckStatus::Satisfied,
        ActionPrerequisite::NodeKind { expected, .. } => {
            if matches_kind(&node.kind.kind, *expected) {
                LiveCheckStatus::Satisfied
            } else {
                LiveCheckStatus::Violated {
                    reason: "observed node kind does not satisfy the frozen condition".into(),
                }
            }
        }
        ActionPrerequisite::Writable(_) if matches_kind(&node.kind.kind, NodeKindClass::Block) => {
            match node.size.block.as_ref().and_then(|block| block.read_only) {
                Some(false) => LiveCheckStatus::Satisfied,
                Some(true) => LiveCheckStatus::Violated {
                    reason: "observed block endpoint is read-only".into(),
                },
                None => unknown("fresh block read-only status is required"),
            }
        }
        ActionPrerequisite::NoUnexpectedConsumers(_) => {
            if observation
                .snapshot
                .graph
                .dependencies()
                .any(|edge| edge.from == target)
                || observation
                    .snapshot
                    .mounts
                    .entries
                    .iter()
                    .any(|mount| match mount.source {
                        MountSource::Filesystem(id)
                        | MountSource::BtrfsSubvolume(id)
                        | MountSource::ZfsDataset(id) => id == target,
                        _ => false,
                    })
            {
                LiveCheckStatus::Violated {
                    reason: "an upper-layer consumer is present in the observation".into(),
                }
            } else {
                // Absence in a partial snapshot cannot establish global exclusivity.
                unknown("complete consumer and mount-namespace coverage is required")
            }
        }
        ActionPrerequisite::NoUnexpectedRelations(_) => {
            if observation
                .snapshot
                .graph
                .relations()
                .any(|edge| edge.from == target || edge.to == target)
            {
                LiveCheckStatus::Violated {
                    reason: "an incident storage relation is present in the observation".into(),
                }
            } else {
                unknown("complete provider relation coverage is required")
            }
        }
        ActionPrerequisite::FilesystemUnmounted(_) => {
            if !matches_kind(&node.kind.kind, NodeKindClass::Filesystem) {
                return LiveCheckStatus::Violated {
                    reason: "correlated node is not a filesystem".into(),
                };
            }
            if observation
                .snapshot
                .mounts
                .entries
                .iter()
                .any(|mount| mount.source == MountSource::Filesystem(target))
            {
                LiveCheckStatus::Violated {
                    reason: "filesystem is mounted in an observed namespace".into(),
                }
            } else {
                unknown("complete mount-namespace coverage and resolved mount sources are required")
            }
        }
        _ => unknown("a supported observed block endpoint is required"),
    }
}

/// Keeps evidence gaps distinct from observed contradictions.
fn unknown(reason: impl Into<String>) -> LiveCheckStatus {
    LiveCheckStatus::Unknown {
        required_evidence: reason.into(),
    }
}

/// Invalid observation envelopes are rejected before they can advance a run.
#[derive(Debug, Error)]
pub enum ObservationError {
    /// Every publication must identify its probe-service instance.
    #[error("observation has a nil probe instance")]
    NilInstance,
    /// Graph relationships must reference existing nodes and obey structural invariants.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// Nil node identities cannot be correlated or used as runtime resources.
    #[error("observation contains a nil node identity")]
    NilNode,
    /// An explicitly stamped node belongs to another publication.
    #[error("node {0:?} belongs to a different observation epoch")]
    NodeEpoch(NodeId),
    /// Typed mount sources must reference nodes in the same snapshot.
    #[error("mount source {0:?} is absent from the observation graph")]
    MountSource(NodeId),
}

impl LiveObservation {
    /// Validates representation only; the trusted host must establish provenance and freshness.
    /// Unstamped nodes and explicit missing placeholders remain valid partial observations.
    pub fn validate(&self) -> Result<(), ObservationError> {
        if self.reference.probe_instance.is_nil() {
            return Err(ObservationError::NilInstance);
        }
        self.snapshot.graph.validate_structure()?;
        for (id, node) in self.snapshot.graph.nodes() {
            if id.as_uuid().is_nil() {
                return Err(ObservationError::NilNode);
            }
            if node
                .size
                .observed_in
                .is_some_and(|epoch| epoch != self.reference.epoch)
            {
                return Err(ObservationError::NodeEpoch(*id));
            }
        }
        for mount in &self.snapshot.mounts.entries {
            if let MountSource::Filesystem(id)
            | MountSource::BtrfsSubvolume(id)
            | MountSource::ZfsDataset(id) = mount.source
                && self.snapshot.graph.node(&id).is_none()
            {
                return Err(ObservationError::MountSource(id));
            }
        }
        Ok(())
    }
}
