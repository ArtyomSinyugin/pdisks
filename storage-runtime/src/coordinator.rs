//! Sequential DAG execution sharing one admission/checkpoint/dispatch path and independent final verification.

use crate::{
    bindings::{BindingError, OutputBindings, verify_activation, verify_created_lv},
    checkpoint::{ActiveAttempt, CheckpointError, CheckpointStore},
    dispatcher::{BackendDispatcher, ExecutionReceipt},
    host::{HostError, RuntimeHost, check_reviewed_plan},
    lease::{ExecutionLease, ExecutionRoot, HostExecutionLock, LeaseError},
};
use storage_engine::{
    ActionId, ActionRunState, BackendCompletion, BackendOutcome, ExecutionRun, ExecutionRunError,
    LiveObservation, editing::OperationId,
};
use thiserror::Error;

/// Rejected execution never falls back to unchecked dispatch or automatic retry.
#[derive(Debug, Error)]
pub enum CoordinatorError {
    /// At least one frozen operation lacks support; the run has not invoked any backend.
    #[error("whole-plan readiness is not satisfied: {0:?}")]
    PlanUnavailable(storage_engine::LiveCheckStatus),
    /// Final success is immutable; an already verified run is never executed again.
    #[error("run is already verified")]
    AlreadyVerified,
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

/// Result of a sequential run attempt; stopped admission and final Unknown are not successful runs.
#[derive(Debug)]
pub struct ExecutionSummary {
    /// Attempt whose actions and final report were evaluated.
    pub run_id: uuid::Uuid,
    /// True only after independent final goal and preservation checks all pass.
    pub verified: bool,
    /// First action that did not become Verified, if the DAG stopped before final verification.
    pub stopped_action: Option<ActionId>,
    /// Latest actual publication, retaining its reference; on early stop it need not be a final probe.
    pub observation: Option<LiveObservation>,
}

/// Owns the only host-side sequence around a synchronous backend call.
/// Native writes require explicit operation capability, scope, authorization, and result evidence.
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

    /// Executes the trusted DAG sequentially, retaining one host lock through fresh final verification.
    /// Only Verified predecessors permit progress; admission Unknown stops without a retry loop.
    /// Already verified actions are skipped only within this same coordinator's attempt and bindings.
    pub fn execute_run(
        &mut self,
        run: &mut ExecutionRun,
    ) -> Result<ExecutionSummary, CoordinatorError> {
        if self.run_id != run.run_id() {
            return Err(CoordinatorError::RunMismatch);
        }
        if self.halted {
            return Err(CoordinatorError::Stopped);
        }
        if run.is_verified() {
            return Err(CoordinatorError::AlreadyVerified);
        }
        check_reviewed_plan(run)?;
        let lock = self.root.try_lock_host()?;
        let store = CheckpointStore::new(&lock);
        if store.inspect()?.is_some() {
            return Err(HostError::ActiveAttempt.into());
        }
        let initial = self.host.probe()?;
        let readiness = self
            .host
            .check_plan(run, &initial, self.dispatcher.backend_id(), &lock)?;
        if readiness != storage_engine::LiveCheckStatus::Satisfied {
            return Err(CoordinatorError::PlanUnavailable(readiness));
        }
        let ordered: Vec<_> = run
            .context()
            .plan
            .ordered_actions()
            .iter()
            .map(|action| action.id)
            .collect();
        for action in ordered {
            match run.state(action)? {
                ActionRunState::Verified => continue,
                ActionRunState::Waiting | ActionRunState::Checking => {}
                _ => return Err(CoordinatorError::Stopped),
            }
            let state = self.execute_action_locked(run, action, None, Some(&lock))?;
            if state != ActionRunState::Verified {
                return Ok(Self::summary(run, Some(action)));
            }
        }
        let observed = self.host.probe()?;
        let report = self
            .host
            .verify_goal(run, &observed, &self.bindings, &lock)?;
        if report.run_id != run.run_id() || report.observation != *observed.observation() {
            return Err(CoordinatorError::InvalidDecision);
        }
        if store.inspect()?.is_some() {
            return Err(HostError::ActiveAttempt.into());
        }
        run.record_goal_verification(report)?;
        Ok(Self::summary(run, None))
    }

    /// Returns evidence already accepted by the engine, never a predicted graph or adapter result.
    fn summary(run: &ExecutionRun, stopped_action: Option<ActionId>) -> ExecutionSummary {
        ExecutionSummary {
            run_id: run.run_id(),
            verified: run.is_verified(),
            stopped_action,
            observation: run.latest_observation().cloned(),
        }
    }

    /// Executes one waiting/checking action, respecting the engine's verified-predecessor rules.
    /// Unknown admission can be checked again with fresh evidence; an invocation is never retried.
    /// Action success is separate from final goal verification performed by execute_run.
    pub fn execute_action(
        &mut self,
        run: &mut ExecutionRun,
        action: ActionId,
        operation: Option<OperationId>,
    ) -> Result<ActionRunState, CoordinatorError> {
        self.execute_action_locked(run, action, operation, None)
    }

    /// Shares the exact action path between individual execution and a run holding continuous exclusion.
    fn execute_action_locked(
        &mut self,
        run: &mut ExecutionRun,
        action: ActionId,
        operation: Option<OperationId>,
        lock: Option<&HostExecutionLock>,
    ) -> Result<ActionRunState, CoordinatorError> {
        if self.run_id != run.run_id() {
            return Err(CoordinatorError::RunMismatch);
        }
        if self.halted {
            return Err(CoordinatorError::Stopped);
        }
        check_reviewed_plan(run)?;
        match run.state(action)? {
            ActionRunState::Waiting => run.begin_check(action)?,
            ActionRunState::Checking => {}
            state => return Err(ExecutionRunError::InvalidTransition { action, state }.into()),
        }
        let lease = match lock {
            Some(lock) => ExecutionLease::under_host_lock(lock, run, action)?,
            None => ExecutionLease::try_acquire(&self.root, run, action)?,
        };
        let store = CheckpointStore::new(lease.host_lock());
        if store.inspect()?.is_some() {
            return Err(HostError::ActiveAttempt.into());
        }
        let observed = self.host.probe()?;
        let decision = self
            .host
            .check_admission(run, action, &observed, &self.bindings, &lease)?;
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
            let verification = if resolved.lv_target().is_some() {
                verify_activation(
                    run,
                    action,
                    &resolved,
                    &self.bindings,
                    observed.observation(),
                    receipt.created_lv_uuid.as_deref(),
                )
                .map(|report| (report, None))
            } else {
                verify_created_lv(
                    run,
                    action,
                    &resolved,
                    &self.bindings,
                    observed.observation(),
                    receipt.created_lv_uuid.as_deref(),
                )
            };
            let (report, candidate) = match verification {
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
