//! Typed host-policy requests shared by Rust embeddings and the Stabby callback channel.

use crate::{IncidentInspection, LiveCheckStatus, LiveObservation, ProtectedScope, host::*};
use authorization::ApprovalRequest;
use serde::Serialize;
use targets::ResolvedAction;

/// Exact kind of exclusion or preservation evidence requested from the embedding application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeCheck {
    /// Complete affected-device and namespace coverage.
    Scope,
    /// No external consumers, writes or DM changes throughout the operation.
    ConsumerExclusion,
    /// Exclusive source, mount target and namespace through verification.
    MountExclusion,
    /// Coverage of the independent final publication.
    FinalScope,
    /// Independent preservation of protected contents and configuration.
    Preservation,
}

/// Borrowed reviewed inputs; this is callback context, never a new executable document.
#[derive(Serialize)]
pub struct PolicyContext<'a> {
    /// Actual attempt requesting the decision.
    pub run_id: uuid::Uuid,
    /// Immutable reviewed plan identity.
    pub plan_id: uuid::Uuid,
    /// Actions reconstructed and validated by the engine.
    pub actions: &'a [storage_engine::PlannedAction],
    /// Dependencies of those exact actions.
    pub dependencies: &'a [storage_engine::ActionDependency],
    /// Historical observations, distinct from fresh evidence.
    pub baseline: &'a storage_engine::HistoricalBaseline,
    /// Requested final state and explicit intents.
    pub goal: &'a crate::model::DesiredState,
    /// Restrictions whose preservation requires independent evidence.
    pub protected_scope: &'a ProtectedScope,
}
impl<'a> From<&ExecutionContext<'a>> for PolicyContext<'a> {
    /// Borrows the coordinator's exact reviewed context without reanalysis or remapping.
    fn from(context: &ExecutionContext<'a>) -> Self {
        Self {
            run_id: context.run_id,
            plan_id: context.plan_id,
            actions: context.plan.actions(),
            dependencies: context.plan.dependencies(),
            baseline: context.historical_baseline,
            goal: context.requested_goal,
            protected_scope: context.protected_scope,
        }
    }
}

/// Trusted side channel; ordinary application requests never contain a policy result.
#[derive(Serialize)]
#[serde(tag = "check", rename_all = "snake_case")]
pub enum PolicyRequest<'a> {
    /// Requires a fresh exact action context or an independent final publication.
    Scope {
        /// Specific evidence required; final checks have no single action.
        kind: ScopeCheck,
        /// Immutable reviewed execution context.
        context: PolicyContext<'a>,
        /// Resolved action; None only for final coverage/preservation.
        action: Option<&'a ResolvedAction>,
        /// Actual publication being evaluated.
        observed: &'a LiveObservation,
    },
    /// General admission and destructive confirmation are separate callback invocations.
    Authorize {
        /// Full digest-, action-, backend- and fingerprint-bound request.
        request: &'a ApprovalRequest,
        /// Some requires explicit consent to replace this target's contents.
        destructive_target: Option<crate::model::NodeId>,
    },
    /// Exact confirmation does not establish native quiescence or authority.
    Incident {
        /// Immutable inspection consciously reviewed by the caller.
        reviewed: &'a IncidentInspection,
        /// New independently collected inspection under the same lock.
        fresh: &'a IncidentInspection,
    },
}

/// Application-supplied authorization and environmental evidence, outside saved plans and JSON calls.
/// Satisfied is an attestation by the host; it must not be inferred merely from running as root.
pub trait NativeHostPolicy {
    /// Decides each exact request synchronously; unknown or unavailable evidence must fail closed.
    fn check(&mut self, request: PolicyRequest<'_>) -> LiveCheckStatus;
}

impl<F> NativeHostPolicy for F
where
    F: for<'a> FnMut(PolicyRequest<'a>) -> LiveCheckStatus,
{
    /// Rust embedders can supply an ordinary closure without implementing transport machinery.
    fn check(&mut self, request: PolicyRequest<'_>) -> LiveCheckStatus {
        self(request)
    }
}
