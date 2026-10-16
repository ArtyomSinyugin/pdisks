//! New LV identities become usable only after receipt, fresh observation, and verified postconditions.

use crate::targets::{ResolvedAction, TargetError, check_scope};
use std::collections::HashMap;
use storage_core::model::{DependencyKind, ExternalId, NodeId, NodeKind, NodeSpec, Presence};
use storage_engine::{
    ActionId, ActionRunState, ExecutionRun, ExecutionRunError, LiveObservation,
    ObservationReference, VerificationReport,
    editing::NodeCorrelation,
    live::{ObservationError, check_observed_postconditions_with_bindings},
};
use storage_provider::{ActionOutput, LvmAction, ProviderAction};
use thiserror::Error;
use uuid::Uuid;

/// Binding failures preserve the active checkpoint and forbid successor dispatch.
#[derive(Debug, Error)]
pub enum BindingError {
    /// A binding belongs to another attempt or publication, or is not yet verified.
    #[error("output binding does not belong to this verified action")]
    NotVerified,
    /// Receipt UUID is absent, ambiguous, preexisting, or points outside the admitted VG.
    #[error("created LV identity or ownership is not established by fresh evidence")]
    Identity,
    /// An actual result changed the reviewed scope beyond the new LV.
    #[error(transparent)]
    Scope(#[from] TargetError),
    /// Runtime state cannot be read safely.
    #[error(transparent)]
    Run(#[from] ExecutionRunError),
    /// Actual observed facts are malformed.
    #[error(transparent)]
    Observation(#[from] ObservationError),
}

/// Correlation candidate kept private until all frozen postconditions have passed.
#[derive(Debug)]
pub(crate) struct PendingLvBinding {
    /// Run that actually obtained the post-backend observation.
    run_id: Uuid,
    /// Action whose new output is being correlated.
    action: ActionId,
    /// Exact retained completion publication used for verification.
    observation: ObservationReference,
    /// Node identity reserved in the reviewed plan.
    planned: NodeId,
    /// New UUID returned by the adapter and independently observed.
    uuid: String,
    /// UUID of the existing VG admitted before dispatch.
    vg_uuid: String,
    /// Requested properties independently checked by the engine before publishing this binding.
    spec: NodeSpec,
}

/// Confirmed new outputs scoped to one attempt; no deserialization or manual insertion exists.
#[derive(Debug)]
pub struct OutputBindings {
    /// Attempt that owns every entry.
    run_id: Uuid,
    /// Only candidates that passed the engine's full verification are inserted here.
    entries: HashMap<NodeId, PendingLvBinding>,
}

impl OutputBindings {
    /// Starts an empty map for one specific attempt; this grants no execution authorization.
    pub fn new(run: &ExecutionRun) -> Self {
        Self {
            run_id: run.run_id(),
            entries: HashMap::new(),
        }
    }
    /// Reports the number of independently verified created outputs.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    /// Reports whether this attempt has any verified created outputs.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolves a verified output again by UUID and owning VG in a fresh observation.
    /// Neither its old transient NodeId nor a matching name suffices after another provider pass.
    pub fn resolve(
        &self,
        run: &ExecutionRun,
        planned: NodeId,
        observed: &LiveObservation,
    ) -> Result<NodeId, BindingError> {
        if self.run_id != run.run_id() {
            return Err(BindingError::NotVerified);
        }
        let binding = self
            .entries
            .get(&planned)
            .ok_or(BindingError::NotVerified)?;
        observed.validate()?;
        if observed.reference.probe_instance != binding.observation.probe_instance
            || observed.reference.epoch < binding.observation.epoch
        {
            return Err(BindingError::Identity);
        }
        let id = find_lv(observed, &binding.uuid, &binding.vg_uuid)?;
        if observed.snapshot.graph.node(&id).map(|node| &node.kind) != Some(&binding.spec) {
            return Err(BindingError::Identity);
        }
        Ok(id)
    }

    /// Publishes only a candidate bound to the exact completion just accepted as Verified.
    pub(crate) fn record_verified(
        &mut self,
        run: &ExecutionRun,
        binding: PendingLvBinding,
    ) -> Result<(), BindingError> {
        if self.run_id != run.run_id()
            || binding.run_id != run.run_id()
            || run.state(binding.action)? != ActionRunState::Verified
            || run
                .completion(binding.action)?
                .map(|completion| completion.observation.reference)
                != Some(binding.observation)
            || self.entries.contains_key(&binding.planned)
            || self
                .entries
                .values()
                .any(|other| other.uuid == binding.uuid)
        {
            return Err(BindingError::NotVerified);
        }
        self.entries.insert(binding.planned, binding);
        Ok(())
    }
}

/// Verifies the retained actual completion, using a provisional UUID mapping only for the new LV.
/// A missing receipt UUID stays unknown; no predicted node is copied into an observation.
pub(crate) fn verify_created_lv(
    run: &ExecutionRun,
    action: ActionId,
    resolved: &ResolvedAction,
    before: &LiveObservation,
    created_uuid: Option<&str>,
) -> Result<(VerificationReport, Option<PendingLvBinding>), BindingError> {
    let completion = run.completion(action)?.ok_or(BindingError::NotVerified)?;
    let context = run.context();
    let contract = &context.plan.actions()[action.get() as usize].informational_contract;
    let mut pending = None;
    let mut correlations = Vec::new();
    if let Some(uuid) = created_uuid {
        let ProviderAction::Lvm(LvmAction::CreateLv {
            planned_node_id, ..
        }) = resolved.operation()
        else {
            return Err(BindingError::Identity);
        };
        let identity = ExternalId::LvmLvUuid(uuid.into());
        if before
            .snapshot
            .graph
            .nodes()
            .any(|(_, node)| node.size.identities.contains(&identity))
        {
            return Err(BindingError::Identity);
        }
        let actual = &completion.observation;
        let observed_id = find_lv(actual, uuid, &resolved.target().vg_uuid)?;
        let spec = contract
            .provided_outputs
            .iter()
            .find_map(|output| match output {
                ActionOutput::Node { id, spec } if id == planned_node_id => Some(spec.clone()),
                _ => None,
            })
            .ok_or(BindingError::Identity)?;
        // Remove only the independently identified new node to compare all remaining actual scope.
        let mut remaining = actual.snapshot.clone();
        let edges: Vec<_> = remaining
            .graph
            .dependencies()
            .filter(|edge| edge.from == observed_id || edge.to == observed_id)
            .cloned()
            .collect();
        if edges.iter().any(|edge| edge.from == observed_id)
            || remaining
                .graph
                .relations()
                .any(|edge| edge.from == observed_id || edge.to == observed_id)
        {
            return Err(BindingError::Identity);
        }
        for edge in edges {
            remaining.graph.remove_dependency(&edge);
        }
        remaining.graph.remove_node(&observed_id);
        check_scope(
            &context.historical_baseline.snapshot,
            &remaining,
            resolved.target().planned_id,
        )?;
        correlations.push(NodeCorrelation {
            source: *planned_node_id,
            target: observed_id,
            identity,
        });
        pending = Some(PendingLvBinding {
            run_id: run.run_id(),
            action,
            observation: actual.reference,
            planned: *planned_node_id,
            uuid: uuid.into(),
            vg_uuid: resolved.target().vg_uuid.clone(),
            spec,
        });
    }
    let report = check_observed_postconditions_with_bindings(
        context.historical_baseline,
        &completion.observation,
        &contract.postconditions,
        &correlations,
    )?;
    Ok((
        VerificationReport {
            run_id: run.run_id(),
            action,
            observation: report.observation,
            checks: report.checks,
        },
        pending,
    ))
}

/// Requires one present LV, one UUID, and one Provides edge from the unique admitted VG UUID.
fn find_lv(observed: &LiveObservation, uuid: &str, vg_uuid: &str) -> Result<NodeId, BindingError> {
    if uuid.trim().is_empty() || uuid.trim() != uuid {
        return Err(BindingError::Identity);
    }
    let identity = ExternalId::LvmLvUuid(uuid.into());
    let mut matches = observed
        .snapshot
        .graph
        .nodes()
        .filter(|(_, node)| node.size.identities.contains(&identity));
    let (id, node) = matches.next().ok_or(BindingError::Identity)?;
    if matches.next().is_some()
        || node.size.presence != Presence::Present
        || !matches!(node.kind.kind, NodeKind::LvmLv(_))
        || node
            .size
            .identities
            .iter()
            .filter(|id| matches!(id, ExternalId::LvmLvUuid(_)))
            .count()
            != 1
    {
        return Err(BindingError::Identity);
    }
    let mut parents = observed
        .snapshot
        .graph
        .dependencies()
        .filter(|edge| edge.to == *id);
    let parent = parents.next().ok_or(BindingError::Identity)?;
    if parents.next().is_some() || parent.kind != DependencyKind::Provides {
        return Err(BindingError::Identity);
    }
    let vg = observed
        .snapshot
        .graph
        .node(&parent.from)
        .ok_or(BindingError::Identity)?;
    if !matches!(vg.kind.kind, NodeKind::LvmVg { .. })
        || vg.size.presence != Presence::Present
        || vg
            .size
            .identities
            .iter()
            .filter(|id| matches!(id, ExternalId::LvmVgUuid(_)))
            .count()
            != 1
        || !vg
            .size
            .identities
            .contains(&ExternalId::LvmVgUuid(vg_uuid.into()))
        || observed
            .snapshot
            .graph
            .nodes()
            .filter(|(_, node)| {
                node.size
                    .identities
                    .contains(&ExternalId::LvmVgUuid(vg_uuid.into()))
            })
            .count()
            != 1
    {
        return Err(BindingError::Identity);
    }
    Ok(*id)
}
