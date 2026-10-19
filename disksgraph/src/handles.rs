//! Instance-owned lookup handles; imported document UUIDs are not application handles.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Opaque handle to an editable session owned by one facade instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub(crate) Uuid);
impl SessionId {
    /// Returns a diagnostic/transport identity, never permission to access another instance.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}
/// Opaque handle to a reviewed immutable document owned by one facade instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanId(pub(crate) Uuid);
impl PlanId {
    /// Returns the facade handle identity, which is independent of a saved plan UUID.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}
/// Opaque handle to a retained execution attempt in its owning process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub(crate) Uuid);
impl RunId {
    /// Matches the run identity carried by its snapshots and events.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}
