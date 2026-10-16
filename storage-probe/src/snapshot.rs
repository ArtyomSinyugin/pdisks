//! Published result of one provider probe pass.

use serde::{Deserialize, Serialize};
use storage_core::model::CurrentState;
use storage_provider::LvmVgObservation;

/// One immutable current-state publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeSnapshot {
    /// Strictly increasing publication generation within one LinuxProbe instance.
    pub generation: u64,
    /// Raw allocation records from the same provider calls as `current`.
    /// Older publications without this field retain unknown capacity.
    #[serde(default)]
    pub lvm_vgs: Vec<LvmVgObservation>,
    /// State assembled from all successful providers.
    pub current: CurrentState,
}
