//! Fresh admission under a run-bound OS lease; no dispatcher is invoked here.

use crate::{
    authorization::{ApprovalRequest, AuthorizationContext},
    bindings::{BindingError, OutputBindings},
    capability::{BackendCapabilityCheck, OperationAvailability},
    checkpoint::{CheckpointError, CheckpointStore, plan_digest},
    lease::ExecutionLease,
    probe::{ProbeObservationError, ProbedObservation},
    targets::{ResolvedAction, resolve_action_with_bindings},
};
use storage_engine::{
    ActionId, ActionRunState, AdmissionReport, ExecutionRun, ExecutionRunError, LiveCheckStatus,
    ObservationReference, PlanDocumentError,
};
use storage_probe::LinuxProbe;
use thiserror::Error;

/// Failures leave admission without permission to dispatch.
#[derive(Debug, Error)]
pub enum HostError {
    /// A previously verified output can no longer be identified safely.
    #[error(transparent)]
    Binding(#[from] BindingError),
    /// The OS lease belongs to another action or attempt.
    #[error("execution lease does not match this run and action")]
    LeaseMismatch,
    /// Supplied publication was not the host's most recent successful provider pass.
    #[error("admission observation is stale or belongs to another probe")]
    ObservationMismatch,
    /// An unresolved attempt blocks every new write.
    #[error("an unresolved active attempt blocks execution")]
    ActiveAttempt,
    /// Provider collection or observed checks failed.
    #[error(transparent)]
    Probe(#[from] ProbeObservationError),
    /// Checkpoint corruption or unsafe metadata cannot be treated as absence.
    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),
    /// An invalid run lifecycle or report cannot grant admission.
    #[error(transparent)]
    Run(#[from] ExecutionRunError),
    /// The immutable reviewed plan could not be canonicalized.
    #[error(transparent)]
    Document(#[from] PlanDocumentError),
}

/// Observed prerequisites plus an endpoint only when all applicability checks have passed.
#[derive(Debug)]
pub struct AdmissionDecision {
    /// Complete report consumed by the engine state machine.
    pub report: AdmissionReport,
    /// Immutable resolved operation, absent when capability, scope, or authorization is incomplete.
    pub resolved: Option<ResolvedAction>,
}

/// Trusted observation and admission boundary shared by all eventual application interfaces.
pub trait RuntimeHost {
    /// Obtains a new provider pass without substituting predicted effects or saved JSON.
    fn probe(&mut self) -> Result<ProbedObservation, HostError>;
    /// Checks prerequisites and applicability while the actual OS lease remains held.
    fn check_admission(
        &mut self,
        run: &ExecutionRun,
        action: ActionId,
        observed: &ProbedObservation,
        bindings: &OutputBindings,
        lease: &ExecutionLease,
    ) -> Result<AdmissionDecision, HostError>;
}

/// Linux host with explicit trusted capability and authorization policies.
/// Recorded adapters stay synthetic; selecting them never converts a fixture into native evidence.
pub struct LinuxLiveHost<'probe, C, A> {
    /// Provider configuration owned by the trusted application.
    probe: LinuxProbe<'probe>,
    /// Operation-specific adapter and complete-coverage checks.
    capability: C,
    /// Invoker and plan-bound approval checks.
    authorization: A,
    /// Last successfully collected publication, invalidated before a new collection attempt.
    last_probe: Option<ObservationReference>,
}

impl<'probe, C, A> LinuxLiveHost<'probe, C, A> {
    /// Requires host-selected providers and policies; there is no implicit allow-all configuration.
    pub fn new(probe: LinuxProbe<'probe>, capability: C, authorization: A) -> Self {
        Self {
            probe,
            capability,
            authorization,
            last_probe: None,
        }
    }
}

impl<C: BackendCapabilityCheck, A: AuthorizationContext> RuntimeHost for LinuxLiveHost<'_, C, A> {
    /// Collects graph and free-space measurements from the same actual provider pass.
    fn probe(&mut self) -> Result<ProbedObservation, HostError> {
        self.last_probe = None;
        let observed = ProbedObservation::collect(&self.probe)?;
        self.last_probe = Some(observed.observation().reference);
        Ok(observed)
    }

    /// Never treats successful discovery, simulation, or root privileges as authorization.
    fn check_admission(
        &mut self,
        run: &ExecutionRun,
        action: ActionId,
        observed: &ProbedObservation,
        bindings: &OutputBindings,
        lease: &ExecutionLease,
    ) -> Result<AdmissionDecision, HostError> {
        if lease.run_id() != run.run_id() || lease.action() != action {
            return Err(HostError::LeaseMismatch);
        }
        if run.state(action)? != ActionRunState::Checking {
            return Err(ExecutionRunError::InvalidTransition {
                action,
                state: run.state(action)?,
            }
            .into());
        }
        let reference = observed.observation().reference;
        if self.last_probe != Some(reference)
            || run.latest_observation().is_some_and(|latest| {
                latest.reference.probe_instance != reference.probe_instance
                    || latest.reference.epoch >= reference.epoch
            })
        {
            return Err(HostError::ObservationMismatch);
        }
        if CheckpointStore::new(lease.host_lock()).inspect()?.is_some() {
            return Err(HostError::ActiveAttempt);
        }
        let context = run.context();
        let planned = &context.plan.actions()[action.get() as usize];
        let correlations = bindings.correlations(run, observed.observation())?;
        let checks = observed
            .check_prerequisites_with_bindings(
                context.historical_baseline,
                &planned.informational_contract.prerequisites,
                &correlations,
            )
            .map_err(ProbeObservationError::from)?
            .checks;
        let (mut applicability, mut resolved) = match self
            .capability
            .availability(&planned.operation, observed.observation())
        {
            OperationAvailability::Unknown(reason) => (
                LiveCheckStatus::Unknown {
                    required_evidence: reason,
                },
                None,
            ),
            OperationAvailability::Unavailable(reason) => {
                (LiveCheckStatus::Violated { reason }, None)
            }
            OperationAvailability::Available(backend) => {
                match resolve_action_with_bindings(
                    run,
                    action,
                    observed.observation(),
                    backend,
                    bindings,
                ) {
                    Ok(resolved) => (LiveCheckStatus::Satisfied, Some(resolved)),
                    Err(error) => (
                        LiveCheckStatus::Violated {
                            reason: error.to_string(),
                        },
                        None,
                    ),
                }
            }
        };
        if let Some(action) = &resolved {
            let scope = self
                .capability
                .check_scope(&context, action, observed.observation());
            applicability = scope;
            if applicability == LiveCheckStatus::Satisfied {
                applicability = observed.check_vg_membership(action.target().observed_id);
            }
            if applicability == LiveCheckStatus::Satisfied {
                if checks
                    .iter()
                    .all(|check| check.status == LiveCheckStatus::Satisfied)
                {
                    applicability = self.authorization.check(&ApprovalRequest {
                        run_id: run.run_id(),
                        plan_id: context.plan_id,
                        action_id: action.action_id(),
                        plan_digest: plan_digest(run.document())?,
                        backend: action.backend().clone(),
                        targets: action.fingerprints(),
                        observation: reference,
                    });
                } else {
                    applicability = LiveCheckStatus::Unknown {
                        required_evidence: "authorization deferred until all prerequisites pass"
                            .into(),
                    };
                }
            }
        }
        if applicability != LiveCheckStatus::Satisfied {
            resolved = None;
        }
        Ok(AdmissionDecision {
            report: AdmissionReport {
                run_id: run.run_id(),
                action,
                observation: observed.observation().clone(),
                applicability,
                held_resources: lease.held_resources().to_vec(),
                checks,
            },
            resolved,
        })
    }
}
