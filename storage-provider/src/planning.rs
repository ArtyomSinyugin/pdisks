//! Provider-owned fragments used to compose a cross-technology plan.

use serde::{Deserialize, Serialize};
use storage_core::model::{Diagnostic, NodeId};

use crate::{ActionPrerequisite, ProviderAction};

/// Action identity scoped to one plan fragment.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocalActionId(u32);

impl LocalActionId {
    /// Creates a fragment-local action identity.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the numeric identity assigned by the provider planner.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// One typed action proposed by a provider planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentAction {
    /// Identity used by local dependencies and external requirements.
    pub id: LocalActionId,
    /// Existing typed provider operation.
    pub operation: ProviderAction,
}

/// Required ordering between two actions in the same fragment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentDependency {
    /// Action that must complete first.
    pub before: LocalActionId,
    /// Action that may run only after `before` is verified.
    pub after: LocalActionId,
    /// Semantic reason why the ordering is required.
    pub reason: DependencyReason,
}

/// Stable categories explaining an action dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyReason {
    /// An earlier action creates an input referenced by the later action.
    InputCreation,
    /// An earlier action makes a required runtime interface available.
    InterfacePreparation,
    /// The order prevents an unsafe intermediate resize state.
    ResizeSafety,
    /// Parent and child mount operations require a defined order.
    MountOrder,
    /// Conflicting access to one real resource domain is serialized.
    ResourceCoordination,
    /// The supported transition recipe explicitly requires the order.
    RequestedOrder,
}

/// Prerequisite owned by one local action but supplied outside its fragment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRequirement {
    /// Local action whose contract contains the prerequisite.
    pub action: LocalActionId,
    /// Exact contract condition that needs external evidence or an output.
    pub prerequisite: ActionPrerequisite,
}

/// User request from which a provider fragment was derived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FragmentOrigin {
    /// A requested structural or property change concerning one node.
    Change {
        /// Existing or planned node primarily affected by the change.
        subject: NodeId,
        /// Short provider-owned description suitable for review output.
        description: String,
    },
    /// An explicit operation from `DesiredState::intents`.
    Intent {
        /// Index of the operation in the immutable requested goal.
        index: usize,
    },
}

/// Provider proposal for one concrete change in a larger execution plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanFragment {
    /// Typed actions with fragment-local identities.
    pub actions: Vec<FragmentAction>,
    /// Ordering required within this fragment.
    pub dependencies: Vec<FragmentDependency>,
    /// Conditions expected from the baseline or another fragment.
    pub external_requirements: Vec<ExternalRequirement>,
    /// Requested change or intent represented by this fragment.
    pub origin: FragmentOrigin,
    /// Provider diagnostics produced while constructing the fragment.
    pub diagnostics: Vec<Diagnostic>,
}

#[cfg(test)]
mod tests {
    use storage_core::model::FilesystemKind;

    use crate::FilesystemAction;

    use super::*;

    /// Fragment serialization preserves typed actions and requirement ownership.
    #[test]
    fn fragment_round_trip_preserves_action_contract_links() {
        let backing = NodeId::new();
        let filesystem = NodeId::new();
        let action_id = LocalActionId::new(1);
        let fragment = PlanFragment {
            actions: vec![FragmentAction {
                id: action_id,
                operation: ProviderAction::Filesystem(FilesystemAction::Create {
                    backing,
                    planned_node_id: filesystem,
                    filesystem: FilesystemKind::Ext4,
                    label: Some("data".to_owned()),
                }),
            }],
            dependencies: Vec::new(),
            external_requirements: vec![ExternalRequirement {
                action: action_id,
                prerequisite: ActionPrerequisite::Writable(backing),
            }],
            origin: FragmentOrigin::Change {
                subject: filesystem,
                description: "create data filesystem".to_owned(),
            },
            diagnostics: Vec::new(),
        };

        let json = serde_json::to_string(&fragment)
            .unwrap_or_else(|error| panic!("serialize plan fragment: {error}"));
        let restored = serde_json::from_str::<PlanFragment>(&json)
            .unwrap_or_else(|error| panic!("deserialize plan fragment: {error}"));

        assert_eq!(restored, fragment);
        assert!(json.contains(r#""provider":"filesystem""#));
    }

    /// Dependency reasons survive serialization as stable readable values.
    #[test]
    fn dependency_reason_has_stable_wire_name() {
        let dependency = FragmentDependency {
            before: LocalActionId::new(1),
            after: LocalActionId::new(2),
            reason: DependencyReason::InterfacePreparation,
        };

        let json = serde_json::to_string(&dependency)
            .unwrap_or_else(|error| panic!("serialize fragment dependency: {error}"));
        assert!(json.contains(r#""reason":"interface_preparation""#));
    }
}
