//! Stable user-operation identities and the single editable list of requests.

use serde::{Deserialize, Serialize};
use storage_core::model::NodeId;
use uuid::Uuid;

use crate::ProtectedScope;

use super::PlanRequest;

/// Persistent user-operation identity, independent of per-build action and fragment indices.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(Uuid);

impl OperationId {
    /// Returns the UUID for persistence and validation at import boundaries.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }

    /// Allocates a UUIDv7 identity once when an operation is added.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for OperationId {
    /// Allocates a fresh identity rather than an empty UUID.
    fn default() -> Self {
        Self::new()
    }
}

/// Session revision, initially zero; future session edits will advance it monotonically.
#[repr(transparent)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct DraftRevision(u64);

impl DraftRevision {
    /// Returns the revision value used for optimistic editing checks.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One editable request with identities retained across rebuilds and parameter changes.
/// Public fields and imported values remain untrusted until preview validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanOperation {
    /// Stable user-operation identity.
    pub id: OperationId,
    /// Original user-level parameters.
    pub request: PlanRequest,
    /// Reserved output node for a creating request; absent for other requests.
    pub created_node: Option<NodeId>,
}

impl PlanOperation {
    /// Allocates operation and output identities once, before any preview calculation.
    pub fn new(request: PlanRequest) -> Self {
        let created_node = request.creates_node().then(NodeId::new);
        Self {
            id: OperationId::new(),
            request,
            created_node,
        }
    }
}

/// Editable requests and preservation policy; predicted storage state lives in the engine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDraft {
    /// User ordering, with stable operation and planned-node identities.
    pub operations: Vec<PlanOperation>,
    /// Explicit restrictions checked throughout predicted transitions.
    pub protected_scope: ProtectedScope,
}

#[cfg(test)]
mod tests {
    use storage_core::model::{
        Bytes, FilesystemKind, LvmLvKind, MountContext, MountEntry, MountSource, OperationIntent,
    };

    use super::*;

    /// Creating requests reserve output IDs once and retain them through edits and JSON.
    #[test]
    fn draft_round_trip_preserves_operations_and_planned_nodes() {
        let vg = NodeId::new();
        let mut lv = PlanOperation::new(PlanRequest::CreateLv {
            vg,
            name: "data".to_owned(),
            size: Bytes::new(1024),
            layout: LvmLvKind::Linear,
            activate: true,
        });
        let original = lv.clone();
        if let PlanRequest::CreateLv { size, .. } = &mut lv.request {
            *size = Bytes::new(2048);
        }
        assert_eq!(lv.id, original.id);
        assert_eq!(lv.created_node, original.created_node);
        let node = lv
            .created_node
            .unwrap_or_else(|| panic!("LV needs a reserved node"));
        let filesystem = PlanOperation::new(PlanRequest::CreateFilesystem {
            backing: node,
            filesystem: FilesystemKind::Ext4,
            label: None,
        });
        assert!(filesystem.created_node.is_some());
        let draft = PlanDraft {
            operations: vec![lv, filesystem],
            ..PlanDraft::default()
        };
        let encoded =
            serde_json::to_string(&draft).unwrap_or_else(|error| panic!("encode draft: {error}"));
        let decoded: PlanDraft =
            serde_json::from_str(&encoded).unwrap_or_else(|error| panic!("decode draft: {error}"));
        assert_eq!(draft, decoded);
        assert_ne!(draft.operations[0].id, draft.operations[1].id);
        let id =
            serde_json::to_value(original.id).unwrap_or_else(|error| panic!("encode ID: {error}"));
        let uuid: Uuid =
            serde_json::from_value(id).unwrap_or_else(|error| panic!("decode UUID: {error}"));
        assert_eq!(uuid.get_version_num(), 7);
        assert_eq!(DraftRevision::default().get(), 0);
    }

    /// Non-creating requests round-trip without allocating a planned node.
    #[test]
    fn non_creating_requests_have_no_output_node() {
        let entry = MountEntry {
            source: MountSource::Tmpfs,
            target: "/data".into(),
            options: Vec::new(),
            context: MountContext::Host,
        };
        for request in [
            PlanRequest::Mount(entry.clone()),
            PlanRequest::PersistMount(entry),
            PlanRequest::Unmount {
                target: "/data".into(),
                context: MountContext::Host,
            },
            PlanRequest::RemovePersistentMount {
                target: "/data".into(),
                context: MountContext::Host,
            },
            PlanRequest::SetFilesystemLabel {
                filesystem: NodeId::new(),
                label: Some("data".to_owned()),
            },
            PlanRequest::Maintenance(OperationIntent::Check {
                target: NodeId::new(),
                force: true,
            }),
        ] {
            let operation = PlanOperation::new(request);
            assert!(operation.created_node.is_none());
            let encoded = serde_json::to_string(&operation)
                .unwrap_or_else(|error| panic!("encode request: {error}"));
            assert_eq!(
                serde_json::from_str::<PlanOperation>(&encoded)
                    .unwrap_or_else(|error| panic!("decode request: {error}")),
                operation
            );
        }
    }
}
