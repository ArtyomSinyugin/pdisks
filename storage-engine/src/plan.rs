//! Immutable cross-provider execution plan.

use serde::{Deserialize, Serialize};

use storage_provider::{
    ActionContract, DependencyReason, FragmentOrigin, LocalActionId, ProviderAction,
};

/// Action identity scoped to one execution plan.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActionId(u32);

impl ActionId {
    /// Creates a global action identity from its stable plan index.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the stable numeric plan index.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Fragment identity assigned during composition.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FragmentId(u32);

impl FragmentId {
    /// Creates a fragment identity from its stable input index.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the stable numeric fragment index.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Address of a provider action within the inputs of one composition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FragmentActionRef {
    /// Input fragment position; not a persistent user operation identity.
    pub fragment: FragmentId,
    /// Provider-assigned identity within that fragment.
    pub action: LocalActionId,
}

/// Explicit ordering supplied before transition and resource validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FragmentOrdering {
    /// Action required to precede the other endpoint.
    pub before: FragmentActionRef,
    /// Action required to follow the other endpoint.
    pub after: FragmentActionRef,
    /// Semantic reason retained in the frozen plan dependency.
    pub reason: DependencyReason,
}

/// One action in a composed execution plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedAction {
    /// Identity used by global dependencies.
    pub id: ActionId,
    /// Existing typed provider operation.
    pub operation: ProviderAction,
    /// Fragment retained for GUI grouping and diagnostics.
    pub fragment: FragmentId,
    /// Original identity assigned by the provider fragment.
    pub local_id: LocalActionId,
    /// User request represented by the source fragment.
    pub origin: FragmentOrigin,
    /// Trusted contract calculated in the final predecessor state.
    pub informational_contract: ActionContract,
}

/// Required ordering between two actions in the global DAG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDependency {
    /// Action that must reach a verified result first.
    pub before: ActionId,
    /// Action that may run after `before`.
    pub after: ActionId,
    /// Semantic reason why the ordering exists.
    pub reason: DependencyReason,
}

/// Frozen result of composing provider fragments into one action DAG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPlan {
    actions: Vec<PlannedAction>,
    dependencies: Vec<ActionDependency>,
}

impl ExecutionPlan {
    /// Creates a plan after structural and transition validation succeeds.
    pub(crate) fn new(actions: Vec<PlannedAction>, dependencies: Vec<ActionDependency>) -> Self {
        Self {
            actions,
            dependencies,
        }
    }

    /// Returns all actions in stable identity order.
    pub fn actions(&self) -> &[PlannedAction] {
        &self.actions
    }

    /// Returns all semantic ordering edges.
    pub fn dependencies(&self) -> &[ActionDependency] {
        &self.dependencies
    }

    /// Returns actions in the deterministic topological order used for validation.
    pub fn ordered_actions(&self) -> Vec<&PlannedAction> {
        crate::compose::topological_order(self.actions.len(), &self.dependencies)
            .unwrap_or_else(|error| unreachable!("frozen plan was structurally validated: {error}"))
            .into_iter()
            .map(|id| &self.actions[id.get() as usize])
            .collect()
    }
}
