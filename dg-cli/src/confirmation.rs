//! Root application policy: one exact-plan confirmation, with ordinary runtime safety checks intact.

use disksgraph::{
    LiveCheckStatus,
    model::NodeId,
    native::{NativeHostPolicy, PolicyRequest, ScopeCheck},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::BufRead;

/// Reviewed execution scope captured before authorization; no policy result is imported from a plan.
#[derive(Serialize)]
struct Review {
    /// Full reviewed actions, dependencies, goal and historical baseline.
    context: Value,
    /// Sole attempt to which consent applies.
    run_id: uuid::Uuid,
    /// Immutable reviewed document.
    plan_id: uuid::Uuid,
    /// Digest of the exact immutable document, independent of display text.
    digest: String,
    /// Same-process challenge displayed before reading input.
    token: uuid::Uuid,
}

/// A confirmation is consent by the root invoker, not a new library authorization mechanism.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirmation {
    /// Explicit affirmative operation; EOF, cancel and other commands refuse execution.
    command: String,
    /// Attempt shown by the review.
    run_id: uuid::Uuid,
    /// Reviewed document identity.
    plan_id: uuid::Uuid,
    /// Exact document digest.
    digest: String,
    /// Challenge from this live review.
    token: uuid::Uuid,
}

/// Root CLI owns an exclusive storage administration interval; review explicitly states this assumption.
/// It never attests native quiescence for incident clear or nonempty protected content preservation.
pub struct RootPolicy<F> {
    /// Root invocation is checked independently of plan or callback contents.
    root: bool,
    /// Transport-specific review/response exchange; CLI and backend4 share all decision logic.
    exchange: F,
    /// Latest reviewed scope supplied by the real coordinator.
    context: Option<Value>,
    /// One consent bound to the exact plan and actual run.
    confirmed: Option<Review>,
    /// Failed/cancelled review is not repeatedly prompted during this attempt.
    refused: bool,
    /// Distinguishes a new explicit apply from repeated checks inside the same run.
    current_run: Option<uuid::Uuid>,
}
impl<F: FnMut(&Value) -> Result<Value, String>> RootPolicy<F> {
    /// Constructs host policy from actual process identity and the caller's confirmation channel.
    pub fn new(exchange: F) -> Self {
        Self::with_root(
            rustix::process::geteuid().is_root() && rustix::process::getuid().is_root(),
            exchange,
        )
    }
    /// Separates the process measurement from deterministic policy tests without changing global UID.
    fn with_root(root: bool, exchange: F) -> Self {
        Self {
            root,
            exchange,
            context: None,
            confirmed: None,
            refused: false,
            current_run: None,
        }
    }
    /// Binds consent to the actual run and exact document; runtime validates action membership.
    fn authorize(
        &mut self,
        request: &disksgraph::host::authorization::ApprovalRequest,
        destructive_target: Option<NodeId>,
    ) -> LiveCheckStatus {
        if self.refused {
            return refused();
        }
        if self.confirmed.is_none() {
            let Some(context) = self.context.take() else {
                return refused();
            };
            let review = Review {
                context,
                run_id: request.run_id,
                plan_id: request.plan_id,
                digest: request.plan_digest.clone(),
                token: uuid::Uuid::now_v7(),
            };
            let displayed = json!({"type":"plan_review", "review":review,
                "exclusive_storage_administration_required":true,
                "confirmation_command":"apply"});
            let confirmation = (self.exchange)(&displayed)
                .ok()
                .and_then(|value| serde_json::from_value::<Confirmation>(value).ok());
            if !confirmation.is_some_and(|answer| {
                answer.command == "apply"
                    && answer.run_id == review.run_id
                    && answer.plan_id == review.plan_id
                    && answer.digest == review.digest
                    && answer.token == review.token
            }) {
                self.refused = true;
                return refused();
            }
            self.confirmed = Some(review);
        }
        let Some(review) = &self.confirmed else {
            return refused();
        };
        if review.run_id != request.run_id
            || review.plan_id != request.plan_id
            || review.digest != request.plan_digest
        {
            return refused();
        }
        if let Some(target) = destructive_target {
            // The supported formatter only targets a verified new LV in this already-reviewed run.
            let target = json!(target);
            if !request
                .targets
                .iter()
                .any(|fingerprint| json!(fingerprint.planned_id) == target)
            {
                return refused();
            }
        }
        LiveCheckStatus::Satisfied
    }
}
impl<F: FnMut(&Value) -> Result<Value, String>> NativeHostPolicy for RootPolicy<F> {
    /// Root supplies authority and conscious confirmation; native facts still come from runtime/providers.
    fn check(&mut self, request: PolicyRequest<'_>) -> LiveCheckStatus {
        if !self.root {
            return refused();
        }
        match request {
            PolicyRequest::Scope { kind, context, .. } => {
                if kind == ScopeCheck::Preservation
                    && (!context.protected_scope.nodes.is_empty()
                        || !context.protected_scope.runtime_mounts.is_empty()
                        || !context.protected_scope.persistent_mounts.is_empty())
                {
                    return LiveCheckStatus::Unknown {
                        required_evidence: "independent protected contents preservation".into(),
                    };
                }
                if self.current_run != Some(context.run_id) {
                    self.current_run = Some(context.run_id);
                    self.confirmed = None;
                    self.refused = false;
                }
                self.context = serde_json::to_value(context).ok();
                LiveCheckStatus::Satisfied
            }
            PolicyRequest::Authorize {
                request,
                destructive_target,
            } => self.authorize(request, destructive_target),
            PolicyRequest::Incident { .. } => LiveCheckStatus::Unknown {
                required_evidence:
                    "independent native backend quiescence after an interrupted operation".into(),
            },
        }
    }
}

/// Refusal leaves the coordinator before dispatch and does not imply rollback of earlier effects.
fn refused() -> LiveCheckStatus {
    LiveCheckStatus::Violated {
        reason: "root invocation and exact live plan confirmation are required".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disksgraph::{
        ActionId, ObservationReference,
        host::{BackendId, authorization::ApprovalRequest, checkpoint::TargetFingerprint},
    };
    use std::{cell::Cell, rc::Rc};

    /// Supplies synthetic context only to exercise confirmation, never a writable runtime host.
    fn request() -> ApprovalRequest {
        ApprovalRequest {
            run_id: uuid::Uuid::now_v7(),
            plan_id: uuid::Uuid::now_v7(),
            action_id: ActionId::new(0),
            plan_digest: "a".repeat(64),
            backend: BackendId::new("confirmation-fixture").unwrap_or_else(|| unreachable!()),
            targets: Vec::new(),
            observation: ObservationReference {
                probe_instance: uuid::Uuid::now_v7(),
                epoch: disksgraph::model::ProbeEpoch::new(1),
            },
        }
    }
    /// Copies only the fields explicitly requested by the displayed same-process review.
    fn answer(review: &Value) -> Value {
        let review = &review["review"];
        json!({"command":"apply", "run_id":review["run_id"],"plan_id":review["plan_id"],"digest":review["digest"],"token":review["token"]})
    }

    /// Wrong challenge, document, attempt, command or extra fields cannot approve execution.
    #[test]
    fn confirmation_requires_exact_review_and_root() {
        for field in ["token", "digest", "plan_id", "run_id", "command", "extra"] {
            let mut policy = RootPolicy::with_root(true, |review: &Value| {
                let mut response = answer(review);
                response[field] = json!("incorrect");
                Ok(response)
            });
            policy.context = Some(json!({"actions":[]}));
            assert!(
                matches!(
                    policy.authorize(&request(), None),
                    LiveCheckStatus::Violated { .. }
                ),
                "{field}"
            );
        }
        let mut policy = RootPolicy::with_root(false, |_: &Value| -> Result<Value, String> {
            panic!("non-root cannot prompt")
        });
        let request = request();
        assert!(matches!(
            policy.check(PolicyRequest::Authorize {
                request: &request,
                destructive_target: None
            }),
            LiveCheckStatus::Violated { .. }
        ));
    }

    /// Confirmation input must be one bounded JSON record; absence and oversized input refuse consent.
    #[test]
    fn confirmation_input_is_bounded_and_explicit() -> Result<(), Box<dyn std::error::Error>> {
        assert!(read_reply(&mut &b""[..]).is_err());
        assert!(read_reply(&mut &b"broken\n"[..]).is_err());
        assert!(read_reply(&mut &vec![b' '; 64 * 1024 + 1][..]).is_err());
        assert_eq!(
            read_reply(&mut &b"{\"command\":\"cancel\"}\n"[..])?["command"],
            "cancel"
        );
        Ok(())
    }

    /// One consent covers one immutable run; altered documents/attempts and unbound destructive targets fail.
    #[test]
    fn confirmed_plan_is_bound_and_not_prompted_again() {
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let mut policy = RootPolicy::with_root(true, move |review: &Value| {
            captured.set(captured.get() + 1);
            Ok(answer(review))
        });
        policy.context = Some(json!({"actions":[]}));
        let mut request = request();
        assert_eq!(policy.authorize(&request, None), LiveCheckStatus::Satisfied);
        request.action_id = ActionId::new(1);
        assert_eq!(policy.authorize(&request, None), LiveCheckStatus::Satisfied);
        let target = NodeId::new();
        assert!(matches!(
            policy.authorize(&request, Some(target)),
            LiveCheckStatus::Violated { .. }
        ));
        request.targets.push(TargetFingerprint {
            planned_id: target,
            external_ids: Vec::new(),
        });
        assert_eq!(
            policy.authorize(&request, Some(target)),
            LiveCheckStatus::Satisfied
        );
        request.plan_digest = "b".repeat(64);
        assert!(matches!(
            policy.authorize(&request, None),
            LiveCheckStatus::Violated { .. }
        ));
        request.plan_digest = "a".repeat(64);
        request.run_id = uuid::Uuid::now_v7();
        assert!(matches!(
            policy.authorize(&request, None),
            LiveCheckStatus::Violated { .. }
        ));
        assert_eq!(calls.get(), 1);
    }
}

/// Reads one bounded confirmation record; EOF or malformed data can never imply consent.
pub fn read_reply(input: &mut impl std::io::BufRead) -> Result<Value, crate::Error> {
    let mut line = Vec::new();
    let count = std::io::Read::take(input, 64 * 1024 + 1).read_until(b'\n', &mut line)?;
    if count == 0 || line.len() > 64 * 1024 {
        return Err(crate::Error::Usage(
            "one bounded explicit confirmation is required".into(),
        ));
    }
    Ok(serde_json::from_slice(&line)?)
}
