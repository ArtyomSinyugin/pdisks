//! Published result of one provider probe pass.

use serde::{Deserialize, Serialize};
use storage_core::model::CurrentState;

/// One immutable current-state publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeSnapshot {
    /// Process-local publication generation.
    pub generation: u64,
    /// State assembled from all successful providers.
    pub current: CurrentState,
}
