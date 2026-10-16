//! Trusted host authorization, deliberately without a deserializable approval flag.

use crate::checkpoint::TargetFingerprint;
use storage_engine::{ActionId, LiveCheckStatus, ObservationReference};
use storage_provider::BackendId;
use uuid::Uuid;

/// Exact request submitted to a host-owned approval policy after fresh target resolution.
#[derive(Debug)]
pub struct ApprovalRequest {
    /// Attempt for which the host is requesting permission.
    pub run_id: Uuid,
    /// Reviewed immutable plan, not a mutable draft.
    pub plan_id: Uuid,
    /// Sole action being authorized.
    pub action_id: ActionId,
    /// SHA-256 of the exact canonical plan document.
    pub plan_digest: String,
    /// Concrete adapter whose capabilities were checked.
    pub backend: BackendId,
    /// UUID-qualified targets whose scope was checked under the held lease.
    pub targets: Vec<TargetFingerprint>,
    /// Actual provider publication used to resolve these targets.
    pub observation: ObservationReference,
}

/// Policy implemented by the trusted application host, never by imported JSON or a root flag.
/// Positive authorization must cover the entire request and any destructive confirmation.
pub trait AuthorizationContext {
    /// Evaluates the actual invoking principal and confirmation against this exact request.
    fn check(&mut self, request: &ApprovalRequest) -> LiveCheckStatus;
}
