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
        let mut indegree = vec![0_usize; self.actions.len()];
        for dependency in &self.dependencies {
            indegree[dependency.after.get() as usize] += 1;
        }
        let mut emitted = vec![false; self.actions.len()];
        let mut ordered = Vec::with_capacity(self.actions.len());
        while ordered.len() < self.actions.len() {
            let Some(next) = indegree
                .iter()
                .enumerate()
                .find_map(|(index, degree)| (!emitted[index] && *degree == 0).then_some(index))
            else {
                break;
            };
            emitted[next] = true;
            ordered.push(&self.actions[next]);
            for dependency in self
                .dependencies
                .iter()
                .filter(|dependency| dependency.before.get() as usize == next)
            {
                indegree[dependency.after.get() as usize] -= 1;
            }
        }
        ordered
    }
}
