//! Single synchronous admission/checkpoint/dispatch/completion path for the first LVM action.

use crate::{
    bindings::{BindingError, OutputBindings, verify_created_lv},
    checkpoint::{ActiveAttempt, CheckpointError, CheckpointStore},
    dispatcher::{BackendDispatcher, ExecutionReceipt},
    host::{HostError, RuntimeHost},
    lease::{ExecutionLease, ExecutionRoot, LeaseError},
};
use storage_engine::{
    ActionId, ActionRunState, BackendCompletion, BackendOutcome, ExecutionRun, ExecutionRunError,
    editing::OperationId,
};
use thiserror::Error;

/// Rejected execution never falls back to unchecked dispatch or automatic retry.
#[derive(Debug, Error)]
pub enum CoordinatorError {
    /// A previous dispatch or clear did not finish with acknowledged verified success.
    #[error("coordinator stopped after an incomplete action; automatic continuation is forbidden")]
    Stopped,
    /// A trusted host returned an endpoint inconsistent with the frozen action or dispatcher.
    #[error("admission endpoint does not match the run, publication, operation, or backend")]
    InvalidDecision,
    /// The coordinator and output bindings belong to a different execution attempt.
    #[error("coordinator belongs to another run")]
    RunMismatch,
    /// Created output identity or actual result could not be established.
    #[error(transparent)]
    Binding(#[from] BindingError),
    /// OS exclusion could not be established.
    #[error(transparent)]
    Lease(#[from] LeaseError),
    /// Fresh host evidence or policies could not be obtained.
    #[error(transparent)]
    Host(#[from] HostError),
    /// Checkpoint publication/removal was not durably acknowledged.
    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),
    /// The engine rejected a report or lifecycle transition.
    #[error(transparent)]
    Run(#[from] ExecutionRunError),
}

/// Owns the only host-side sequence around a synchronous backend call.
/// Native writes remain unavailable until capability, target, and result adapters are implemented.
pub struct ExecutionCoordinator<H, D> {
    /// Shared configured execution root; different roots cannot coordinate with each other.
    root: ExecutionRoot,
    /// Fresh observations and trusted admission policies.
    host: H,
    /// Concrete synchronous adapter, checked against admission before any invocation.
    dispatcher: D,
    /// Latched before invocation and cleared only after verified result plus durable checkpoint clear.
    halted: bool,
    /// Attempt that owns this coordinator and its verified output identities.
    run_id: uuid::Uuid,
    /// Created nodes remain unavailable until complete postcondition verification.
    bindings: OutputBindings,
}

impl<H: RuntimeHost, D: BackendDispatcher> ExecutionCoordinator<H, D> {
    /// Requires explicit host policies and dispatcher; no default write configuration exists.
    pub fn new(root: ExecutionRoot, host: H, dispatcher: D, run: &ExecutionRun) -> Self {
        Self {
            root,
            host,
            dispatcher,
            halted: false,
            run_id: run.run_id(),
            bindings: OutputBindings::new(run),
        }
    }

    /// Returns only outputs whose retained completion passed full postcondition verification.
    pub fn bindings(&self) -> &OutputBindings {
        &self.bindings
    }

    /// Executes one waiting/checking action, respecting the engine's verified-predecessor rules.
    /// Unknown admission can be checked again with fresh evidence; an invocation is never retried.
    /// Action success is separate from final goal verification, which remains unavailable here.
    pub fn execute_action(
        &mut self,
        run: &mut ExecutionRun,
        action: ActionId,
        operation: Option<OperationId>,
    ) -> Result<ActionRunState, CoordinatorError> {
        if self.run_id != run.run_id() {
            return Err(CoordinatorError::RunMismatch);
        }
        if self.halted {
            return Err(CoordinatorError::Stopped);
        }
        match run.state(action)? {
            ActionRunState::Waiting => run.begin_check(action)?,
            ActionRunState::Checking => {}
            state => return Err(ExecutionRunError::InvalidTransition { action, state }.into()),
        }
        let lease = ExecutionLease::try_acquire(&self.root, run, action)?;
        let store = CheckpointStore::new(lease.host_lock());
        if store.inspect()?.is_some() {
            return Err(HostError::ActiveAttempt.into());
        }
        let observed = self.host.probe()?;
        let decision = self.host.check_admission(run, action, &observed, &lease)?;
        if let Some(resolved) = &decision.resolved
            && (resolved.run_id() != run.run_id()
                || resolved.action_id() != action
                || resolved.observation() != decision.report.observation.reference
                || resolved.observation() != observed.observation().reference
                || resolved.operation()
                    != &run.context().plan.actions()[action.get() as usize].operation
                || resolved.backend() != self.dispatcher.backend_id())
        {
            return Err(CoordinatorError::InvalidDecision);
        }
        if !run.record_admission(decision.report)? {
            return Ok(run.state(action)?);
        }
        let Some(resolved) = decision.resolved else {
            run.mark_pre_dispatch_failed(
                action,
                "admitted report has no resolved endpoint".into(),
            )?;
            return Err(CoordinatorError::InvalidDecision);
        };
        let prepared = ActiveAttempt::capture(
            run,
            action,
            operation,
            resolved.fingerprints(),
            resolved.backend().clone(),
        )
        .and_then(|attempt| store.arm(&attempt));
        if let Err(error) = prepared {
            run.mark_pre_dispatch_failed(action, error.to_string())?;
            return Err(error.into());
        }
        // Every exit after this boundary retains the stop latch unless clear is acknowledged.
        self.halted = true;
        run.mark_dispatch_started(action)?;
        let receipt = self
            .dispatcher
            .execute(&resolved, &mut |detail| run.record_progress(action, detail))
            .unwrap_or_else(|error| ExecutionReceipt {
                outcome: BackendOutcome::OutcomeUnknown {
                    reason: error.to_string(),
                },
                created_lv_uuid: None,
            });
        let post = match self.host.probe() {
            Ok(post) => post,
            Err(error) => {
                run.mark_outcome_unknown(action, error.to_string())?;
                return Err(error.into());
            }
        };
        if let Err(error) = run.record_completion(BackendCompletion {
            run_id: run.run_id(),
            action,
            outcome: receipt.outcome,
            observation: post.into_observation(),
        }) {
            run.mark_outcome_unknown(action, error.to_string())?;
            return Err(error.into());
        }
        let mut pending = None;
        if run.state(action)? == ActionRunState::Verifying {
            let (report, candidate) = match verify_created_lv(
                run,
                action,
                &resolved,
                observed.observation(),
                receipt.created_lv_uuid.as_deref(),
            ) {
                Ok(result) => result,
                Err(error) => {
                    run.mark_outcome_unknown(action, error.to_string())?;
                    return Err(error.into());
                }
            };
            run.record_verification(report)?;
            pending = candidate;
        }
        let state = run.state(action)?;
        if state == ActionRunState::Verified {
            if let Some(binding) = pending {
                self.bindings.record_verified(run, binding)?;
            }
            store.clear(run.run_id(), action)?;
            self.halted = false;
        }
        Ok(state)
    }
}
