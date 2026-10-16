//! Checks over actual observations; these do not grant authorization or acquire resources.

use storage_core::model::{GraphError, MountSource, NodeId};
use thiserror::Error;

use crate::LiveObservation;

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
