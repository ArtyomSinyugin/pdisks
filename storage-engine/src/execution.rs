//! Execution lifecycle and trusted live-check contracts; no backend dispatch is implemented.

use serde::{Deserialize, Serialize};
use storage_core::model::{CurrentState, DesiredState, ProbeEpoch};
use storage_provider::{ActionPrerequisite, Postcondition, ResourceAccess, ResourceAccessMode};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    ActionId, ExecutionPlan, GoalAspect, HistoricalBaseline, PlanDocument, PlanDocumentError,
    PlannedAction, ProtectedScope,
};

/// One action's runtime lifecycle, never stored in the immutable plan document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionRunState {
    /// No attempt has begun.
    Waiting,
    /// Live evidence is being collected before any invocation attempt.
    Checking,
    /// Admission passed and the invocation attempt was recorded; its effect is still unverified.
    Running,
    /// Backend reported success and fresh observations await postcondition verification.
    Verifying,
    /// Every required postcondition passed against the post-backend observation.
    Verified,
    /// A known admission, backend, or verification failure stops ordinary successors.
    Failed,
    /// Backend outcome or postcondition evidence is unknown; automatic retry is forbidden.
    OutcomeUnknown,
}

/// Three-valued live evidence, deliberately without a predicted-effect variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveCheckStatus {
    /// The trusted checker established this condition from actual live evidence.
    Satisfied,
    /// A known contradiction prevents admission or successful verification.
    Violated {
        /// Explanation of the observed contradiction.
        reason: String,
    },
    /// Evidence is incomplete; simulation is not an acceptable substitute.
    Unknown {
        /// Evidence still needed from the runtime environment.
        required_evidence: String,
    },
}

/// A typed frozen requirement and its actual live-check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveCheck<T> {
    /// Exact prerequisite, postcondition, or goal aspect being checked.
    pub condition: T,
    /// Result supplied by the trusted runtime checker.
    pub status: LiveCheckStatus,
}

/// Correlation reference from the actual probe service, separate from a historical baseline UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationReference {
    /// Probe-service instance supplying the observation.
    pub probe_instance: Uuid,
    /// Monotonically increasing publication generation within that instance.
    pub epoch: ProbeEpoch,
}

/// Actual published observation supplied by the trusted runtime host.
/// Constructing this value is not itself evidence that a probe took place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveObservation {
    /// Service and generation binding this snapshot to one actual observation.
    pub reference: ObservationReference,
    /// Newly observed state; predicted effects must never be copied into this field.
    pub snapshot: CurrentState,
}

/// Complete live admission supplied immediately before the backend invocation boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionReport {
    /// Run owning this evidence.
    pub run_id: Uuid,
    /// Action whose exact frozen requirements were checked.
    pub action: ActionId,
    /// Fresh observation used by the checker.
    pub observation: LiveObservation,
    /// Fresh identity correlation, capabilities, geometry, and approved-scope applicability.
    /// Refining a target must not silently change the reviewed operation or destructive scope.
    pub applicability: LiveCheckStatus,
    /// Resource leases actually acquired by the host and retained through verification.
    /// This declaration does not implement OS locking or prevent external processes changing disks.
    pub held_resources: Vec<ResourceAccess>,
    /// Every frozen prerequisite, including conditions that simulation had already satisfied.
    pub checks: Vec<LiveCheck<ActionPrerequisite>>,
}

/// Backend result after actual completion, not merely an asynchronous launch acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendOutcome {
    /// Backend reports that the operation completed; postconditions still need verification.
    Succeeded,
    /// Backend reports a known failure.
    Failed {
        /// Provider-owned explanation of the failure.
        reason: String,
    },
    /// Interruption or partial outcome requires observation and explicit recovery.
    OutcomeUnknown {
        /// Provider-owned explanation of the unknown result.
        reason: String,
    },
}

/// Actual backend completion plus a newly published observation, including on failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendCompletion {
    /// Run that invoked the backend.
    pub run_id: Uuid,
    /// Action whose backend operation completed.
    pub action: ActionId,
    /// Classified backend outcome.
    pub outcome: BackendOutcome,
    /// New probe after backend completion; no predicted effect is applied to this snapshot.
    pub observation: LiveObservation,
}

/// Actual postcondition checks tied to the retained post-backend observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    /// Run owning the verification.
    pub run_id: Uuid,
    /// Action whose full postcondition set was checked.
    pub action: ActionId,
    /// Exact observation reference retained in this action's backend completion.
    pub observation: ObservationReference,
    /// Every frozen postcondition, rather than a generic backend-success flag.
    pub checks: Vec<LiveCheck<Postcondition>>,
}

/// Separate final verification of the actual requested configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalVerificationReport {
    /// Run whose complete requested goal was checked.
    pub run_id: Uuid,
    /// Fresh final observation after all actions reached `Verified`.
    pub observation: LiveObservation,
    /// Every requested goal aspect, including persistent configuration and boot when managed.
    pub checks: Vec<LiveCheck<GoalAspect>>,
    /// Actual preservation of the reviewed protected area, not just final graph equality.
    pub preservation: LiveCheckStatus,
}

/// Immutable reviewed context supplied to the trusted runtime checker.
pub struct ExecutionContext<'a> {
    /// Identity of this execution attempt.
    pub run_id: Uuid,
    /// Identity of the reviewed description.
    pub plan_id: Uuid,
    /// Trusted frozen actions, dependencies, and contracts.
    pub plan: &'a ExecutionPlan,
    /// Saved observations used for correlation only, never treated as fresh evidence.
    pub historical_baseline: &'a HistoricalBaseline,
    /// Original goal, including explicit intents whose operations must actually run.
    pub requested_goal: &'a DesiredState,
    /// Reviewed preservation restrictions.
    pub protected_scope: &'a ProtectedScope,
}

/// Runtime host contract for fresh correlation, admission, and actual-result verification.
/// Implementations must probe real state, acquire/retain resource leases, resolve outputs,
/// and check the exact frozen scope. They must never convert simulation checks into live success.
/// The engine currently supplies this boundary only; it does not invoke disk-writing backends.
pub trait LiveExecutionChecks {
    /// Checks all prerequisites immediately before an actual invocation attempt.
    fn check_admission(
        &mut self,
        context: &ExecutionContext<'_>,
        action: &PlannedAction,
        latest_observation: Option<&LiveObservation>,
    ) -> Result<AdmissionReport, String>;

    /// Checks every postcondition using the immutable post-backend observation and provider results.
    fn verify_action(
        &mut self,
        context: &ExecutionContext<'_>,
        action: &PlannedAction,
        completion: &BackendCompletion,
    ) -> Result<VerificationReport, String>;

    /// Verifies the final actual goal, performed intents, and preservation after all actions succeed.
    fn verify_goal(
        &mut self,
        context: &ExecutionContext<'_>,
        latest_observation: Option<&LiveObservation>,
    ) -> Result<GoalVerificationReport, String>;
}

/// Serializable event kind for progress subscribers and reconnecting clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEventKind {
    /// One action changed lifecycle state.
    ActionState {
        /// Action whose state changed.
        action: ActionId,
        /// Newly published runtime state.
        state: ActionRunState,
    },
    /// Provider-owned progress, which never verifies an effect.
    Progress {
        /// Action reporting progress.
        action: ActionId,
        /// Provider-owned progress description.
        detail: String,
    },
    /// Complete live evidence was evaluated without necessarily admitting an invocation.
    Admission {
        /// Action whose evidence was checked.
        action: ActionId,
        /// Whether this report established all admission conditions.
        admitted: bool,
    },
    /// Separate final goal-verification result.
    GoalVerification {
        /// Whether all actual goal and preservation checks succeeded.
        verified: bool,
    },
}

/// Ordered run event; transport request IDs are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    /// Run to which this event belongs.
    pub run_id: Uuid,
    /// Strictly increasing sequence starting at one, used for replay after reconnect.
    pub sequence: u64,
    /// Progress or lifecycle update.
    pub kind: RunEventKind,
}

/// Retained evidence and state for one action, mutable only through run transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActionRun {
    /// Current lifecycle state.
    state: ActionRunState,
    /// Most recent complete live admission report.
    admission: Option<AdmissionReport>,
    /// Classified backend result and its actual observation.
    completion: Option<BackendCompletion>,
    /// Complete actual postcondition report.
    verification: Option<VerificationReport>,
    /// Failure to observe or classify an invocation result.
    unknown_reason: Option<String>,
}

/// One serial execution attempt over an immutable reviewed description.
/// It has no deserializer, backend dispatcher, authorization object, or automatic retry path.
#[derive(Debug)]
pub struct ExecutionRun {
    /// Fresh identity independent of the immutable plan identity.
    run_id: Uuid,
    /// Frozen reviewed goal, historical context, and protection policy.
    document: PlanDocument,
    /// Plan rebuilt from trusted provider contracts at construction.
    plan: ExecutionPlan,
    /// Runtime evidence indexed by stable global action ID.
    actions: Vec<ActionRun>,
    /// Actual most recently published observation, never a projection.
    latest_observation: Option<LiveObservation>,
    /// Exact requested aspects that final live verification must cover.
    goal_aspects: Vec<GoalAspect>,
    /// Most recent complete final verification report.
    goal_verification: Option<GoalVerificationReport>,
    /// Ordered events retained for reconnecting clients.
    events: Vec<RunEvent>,
}

/// Rejected runtime transition or evidence report; no backend is invoked on errors.
#[derive(Debug, Error)]
pub enum ExecutionRunError {
    /// Historical description is invalid before a run is created.
    #[error(transparent)]
    Document(#[from] PlanDocumentError),
    /// Requested action identity is not in this frozen plan.
    #[error("unknown action {0:?}")]
    UnknownAction(ActionId),
    /// The action is not in the lifecycle state required by this transition.
    #[error("action {action:?} cannot transition from {state:?}")]
    InvalidTransition {
        /// Action whose state is incompatible.
        action: ActionId,
        /// Current action state.
        state: ActionRunState,
    },
    /// An ordinary dependency has not reached verified success.
    #[error("action {action:?} waits for verified predecessor {predecessor:?}")]
    PredecessorNotVerified {
        /// Action requesting admission.
        action: ActionId,
        /// Predecessor blocking this action.
        predecessor: ActionId,
    },
    /// The minimal runtime model allows one active action at a time.
    #[error("another action is checking, running, or verifying")]
    ActiveAction,
    /// A failure or unknown outcome forbids ordinary continuation and automatic retry.
    #[error("run stopped after failed or unknown action outcome")]
    RunStopped,
    /// The report belongs to another run, action, or retained post-backend observation.
    #[error("live report does not match this run, action, or observation")]
    ReportMismatch,
    /// Missing or duplicate condition results cannot stand in for a complete check.
    #[error("live report must cover exactly the frozen condition set")]
    IncompleteChecks,
    /// A report has not acquired every resource needed by the frozen contract.
    #[error("live admission is missing a required resource lease")]
    MissingResources,
    /// A new report reuses an old generation or changes probe-service instance mid-run.
    #[error("live observation is stale or belongs to another probe instance")]
    StaleObservation,
    /// The final goal cannot be verified before all actions have verified results.
    #[error("final verification requires every action to be Verified")]
    ActionsNotVerified,
}

impl ExecutionRun {
    /// Creates a waiting attempt after historical validation; this never dispatches a backend.
    pub fn new(document: &PlanDocument) -> Result<Self, ExecutionRunError> {
        let analysis = document.analyse_historical()?;
        let actions = analysis
            .plan
            .actions()
            .iter()
            .map(|_| ActionRun {
                state: ActionRunState::Waiting,
                admission: None,
                completion: None,
                verification: None,
                unknown_reason: None,
            })
            .collect();
        Ok(Self {
            run_id: Uuid::now_v7(),
            document: document.clone(),
            plan: analysis.plan,
            actions,
            latest_observation: None,
            goal_aspects: analysis
                .report
                .goal_checks
                .into_iter()
                .map(|check| check.aspect)
                .collect(),
            goal_verification: None,
            events: Vec::new(),
        })
    }

    /// Returns this attempt's identity.
    pub const fn run_id(&self) -> Uuid {
        self.run_id
    }

    /// Returns the frozen description for durable recording, never a mutable editing draft.
    pub const fn document(&self) -> &PlanDocument {
        &self.document
    }

    /// Returns the immutable reviewed context for the trusted runtime checker.
    pub fn context(&self) -> ExecutionContext<'_> {
        ExecutionContext {
            run_id: self.run_id,
            plan_id: self.document.plan_id,
            plan: &self.plan,
            historical_baseline: &self.document.baseline,
            requested_goal: &self.document.requested_goal,
            protected_scope: &self.document.protected_scope,
        }
    }

    /// Returns an action's current runtime state without granting mutation access.
    pub fn state(&self, action: ActionId) -> Result<ActionRunState, ExecutionRunError> {
        Ok(self.action(action)?.state)
    }

    /// Returns the most recent actual observation, including after partial or failed outcomes.
    pub fn latest_observation(&self) -> Option<&LiveObservation> {
        self.latest_observation.as_ref()
    }

    /// Returns retained admission evidence for review or a runtime status snapshot.
    pub fn admission(
        &self,
        action: ActionId,
    ) -> Result<Option<&AdmissionReport>, ExecutionRunError> {
        Ok(self.action(action)?.admission.as_ref())
    }

    /// Returns the classified backend outcome and actual post-backend snapshot.
    pub fn completion(
        &self,
        action: ActionId,
    ) -> Result<Option<&BackendCompletion>, ExecutionRunError> {
        Ok(self.action(action)?.completion.as_ref())
    }

    /// Returns retained complete postcondition evidence.
    pub fn verification(
        &self,
        action: ActionId,
    ) -> Result<Option<&VerificationReport>, ExecutionRunError> {
        Ok(self.action(action)?.verification.as_ref())
    }

    /// Returns why an invocation was left unclassified when a new probe could not be obtained.
    pub fn unknown_reason(&self, action: ActionId) -> Result<Option<&str>, ExecutionRunError> {
        Ok(self.action(action)?.unknown_reason.as_deref())
    }

    /// Returns ordered events after a client's last consumed sequence.
    pub fn events_since(&self, sequence: u64) -> impl Iterator<Item = &RunEvent> {
        self.events
            .iter()
            .filter(move |event| event.sequence > sequence)
    }

    /// Begins read-only live admission only after required predecessors are verified.
    pub fn begin_check(&mut self, action: ActionId) -> Result<(), ExecutionRunError> {
        self.require_state(action, ActionRunState::Waiting)?;
        if self.actions.iter().any(|action| {
            matches!(
                action.state,
                ActionRunState::Failed | ActionRunState::OutcomeUnknown
            )
        }) {
            return Err(ExecutionRunError::RunStopped);
        }
        for dependency in self
            .plan
            .dependencies()
            .iter()
            .filter(|edge| edge.after == action)
        {
            if self.action(dependency.before)?.state != ActionRunState::Verified {
                return Err(ExecutionRunError::PredecessorNotVerified {
                    action,
                    predecessor: dependency.before,
                });
            }
        }
        if self.actions.iter().any(|action| {
            matches!(
                action.state,
                ActionRunState::Checking | ActionRunState::Running | ActionRunState::Verifying
            )
        }) {
            return Err(ExecutionRunError::ActiveAction);
        }
        self.set_state(action, ActionRunState::Checking);
        Ok(())
    }

    /// Records complete live evidence and, on admission, records the invocation attempt.
    /// Unknown evidence stays in `Checking`; a known violation stops the run before invocation.
    /// The host must dispatch immediately under retained leases; this method never calls a backend.
    pub fn record_admission(&mut self, report: AdmissionReport) -> Result<bool, ExecutionRunError> {
        let action = report.action;
        self.require_state(action, ActionRunState::Checking)?;
        self.check_report_identity(report.run_id)?;
        self.check_fresh_observation(&report.observation)?;
        let contract = &self.plan.actions()[action.get() as usize].informational_contract;
        if !checks_cover(&contract.prerequisites, &report.checks) {
            return Err(ExecutionRunError::IncompleteChecks);
        }
        if contract.affected_resources.iter().any(|required| {
            !report.held_resources.iter().any(|held| {
                held.resource == required.resource
                    && (held.mode == ResourceAccessMode::Write || held.mode == required.mode)
            })
        }) {
            return Err(ExecutionRunError::MissingResources);
        }
        let statuses = std::iter::once(&report.applicability)
            .chain(report.checks.iter().map(|check| &check.status));
        let state = evidence_state(statuses, ActionRunState::Running, ActionRunState::Checking);
        let admitted = state == ActionRunState::Running;
        self.latest_observation = Some(report.observation.clone());
        self.actions[action.get() as usize].admission = Some(report);
        self.push_event(RunEventKind::Admission { action, admitted });
        if state != ActionRunState::Checking {
            self.set_state(action, state);
        }
        Ok(admitted)
    }

    /// Records backend completion and a newer actual snapshot before any postcondition success.
    /// Failure and unknown outcomes retain the new snapshot and block ordinary continuation.
    pub fn record_completion(
        &mut self,
        completion: BackendCompletion,
    ) -> Result<(), ExecutionRunError> {
        let action = completion.action;
        self.require_state(action, ActionRunState::Running)?;
        self.check_report_identity(completion.run_id)?;
        self.check_fresh_observation(&completion.observation)?;
        let state = match completion.outcome {
            BackendOutcome::Succeeded => ActionRunState::Verifying,
            BackendOutcome::Failed { .. } => ActionRunState::Failed,
            BackendOutcome::OutcomeUnknown { .. } => ActionRunState::OutcomeUnknown,
        };
        self.latest_observation = Some(completion.observation.clone());
        self.actions[action.get() as usize].completion = Some(completion);
        self.set_state(action, state);
        Ok(())
    }

    /// Verifies every frozen postcondition against the retained post-backend observation.
    pub fn record_verification(
        &mut self,
        report: VerificationReport,
    ) -> Result<bool, ExecutionRunError> {
        let action = report.action;
        self.require_state(action, ActionRunState::Verifying)?;
        self.check_report_identity(report.run_id)?;
        if self
            .action(action)?
            .completion
            .as_ref()
            .map(|completion| completion.observation.reference)
            != Some(report.observation)
        {
            return Err(ExecutionRunError::ReportMismatch);
        }
        let contract = &self.plan.actions()[action.get() as usize].informational_contract;
        if !checks_cover(&contract.postconditions, &report.checks) {
            return Err(ExecutionRunError::IncompleteChecks);
        }
        let state = evidence_state(
            report.checks.iter().map(|check| &check.status),
            ActionRunState::Verified,
            ActionRunState::OutcomeUnknown,
        );
        self.actions[action.get() as usize].verification = Some(report);
        self.set_state(action, state);
        Ok(state == ActionRunState::Verified)
    }

    /// Stops an invocation when completion or fresh post-backend observation cannot be obtained.
    /// The last actual snapshot remains unchanged; recovery requires a new probe and explicit policy.
    pub fn mark_outcome_unknown(
        &mut self,
        action: ActionId,
        reason: String,
    ) -> Result<(), ExecutionRunError> {
        let state = self.action(action)?.state;
        if !matches!(state, ActionRunState::Running | ActionRunState::Verifying) {
            return Err(ExecutionRunError::InvalidTransition { action, state });
        }
        self.actions[action.get() as usize].unknown_reason = Some(reason);
        self.set_state(action, ActionRunState::OutcomeUnknown);
        Ok(())
    }

    /// Records provider progress without changing state or projecting an effect.
    pub fn record_progress(
        &mut self,
        action: ActionId,
        detail: String,
    ) -> Result<(), ExecutionRunError> {
        self.require_state(action, ActionRunState::Running)?;
        self.push_event(RunEventKind::Progress { action, detail });
        Ok(())
    }

    /// Records separate final actual-goal evidence; action success alone cannot verify the run.
    pub fn record_goal_verification(
        &mut self,
        report: GoalVerificationReport,
    ) -> Result<bool, ExecutionRunError> {
        if !self
            .actions
            .iter()
            .all(|action| action.state == ActionRunState::Verified)
        {
            return Err(ExecutionRunError::ActionsNotVerified);
        }
        self.check_report_identity(report.run_id)?;
        self.check_fresh_observation(&report.observation)?;
        if !checks_cover(&self.goal_aspects, &report.checks) {
            return Err(ExecutionRunError::IncompleteChecks);
        }
        let verified = report.preservation == LiveCheckStatus::Satisfied
            && report
                .checks
                .iter()
                .all(|check| check.status == LiveCheckStatus::Satisfied);
        self.latest_observation = Some(report.observation.clone());
        self.goal_verification = Some(report);
        self.push_event(RunEventKind::GoalVerification { verified });
        Ok(verified)
    }

    /// Returns final goal and preservation evidence separately from action postconditions.
    pub fn goal_verification(&self) -> Option<&GoalVerificationReport> {
        self.goal_verification.as_ref()
    }

    /// Reports actual verified run success only after a complete separate final report.
    pub fn is_verified(&self) -> bool {
        self.goal_verification.as_ref().is_some_and(|report| {
            report.preservation == LiveCheckStatus::Satisfied
                && report
                    .checks
                    .iter()
                    .all(|check| check.status == LiveCheckStatus::Satisfied)
        })
    }

    /// Looks up an action before any use of its numeric index.
    fn action(&self, action: ActionId) -> Result<&ActionRun, ExecutionRunError> {
        self.actions
            .get(action.get() as usize)
            .ok_or(ExecutionRunError::UnknownAction(action))
    }

    /// Requires one exact lifecycle predecessor without mutating the run on failure.
    fn require_state(
        &self,
        action: ActionId,
        expected: ActionRunState,
    ) -> Result<(), ExecutionRunError> {
        let state = self.action(action)?.state;
        if state != expected {
            return Err(ExecutionRunError::InvalidTransition { action, state });
        }
        Ok(())
    }

    /// Binds a report to this attempt rather than another attempt over the same plan.
    fn check_report_identity(&self, run_id: Uuid) -> Result<(), ExecutionRunError> {
        if run_id != self.run_id {
            return Err(ExecutionRunError::ReportMismatch);
        }
        Ok(())
    }

    /// Requires a new publication from the same actual probe-service instance throughout the run.
    fn check_fresh_observation(
        &self,
        observation: &LiveObservation,
    ) -> Result<(), ExecutionRunError> {
        let reference = observation.reference;
        if reference.probe_instance.is_nil()
            || self.latest_observation.as_ref().is_some_and(|latest| {
                latest.reference.probe_instance != reference.probe_instance
                    || latest.reference.epoch.get() >= reference.epoch.get()
            })
        {
            return Err(ExecutionRunError::StaleObservation);
        }
        Ok(())
    }

    /// Publishes one validated state change with a monotonically ordered event.
    fn set_state(&mut self, action: ActionId, state: ActionRunState) {
        self.actions[action.get() as usize].state = state;
        self.push_event(RunEventKind::ActionState { action, state });
    }

    /// Appends a reconnectable event; Vec allocation bounds keep its sequence below u64::MAX.
    fn push_event(&mut self, kind: RunEventKind) {
        self.events.push(RunEvent {
            run_id: self.run_id,
            sequence: self.events.len() as u64 + 1,
            kind,
        });
    }
}

/// Requires exactly the trusted condition set, rejecting extra, missing, or repeated attestations.
fn checks_cover<T: PartialEq>(required: &[T], checks: &[LiveCheck<T>]) -> bool {
    required
        .iter()
        .all(|condition| checks.iter().any(|check| check.condition == *condition))
        && checks.iter().enumerate().all(|(index, check)| {
            required.contains(&check.condition)
                && !checks[..index]
                    .iter()
                    .any(|earlier| earlier.condition == check.condition)
        })
}

/// Known violations dominate unknown evidence; only all-satisfied evidence permits success.
fn evidence_state<'a>(
    statuses: impl Iterator<Item = &'a LiveCheckStatus>,
    satisfied: ActionRunState,
    unknown: ActionRunState,
) -> ActionRunState {
    let mut result = satisfied;
    for status in statuses {
        match status {
            LiveCheckStatus::Violated { .. } => return ActionRunState::Failed,
            LiveCheckStatus::Unknown { .. } => result = unknown,
            LiveCheckStatus::Satisfied => {}
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use storage_core::model::NodeId;

    use super::*;

    /// Synthetic host observation for lifecycle tests; no probe or disk-writing backend is called.
    fn observation(run: &ExecutionRun, instance: Uuid, epoch: u64) -> LiveObservation {
        LiveObservation {
            reference: ObservationReference {
                probe_instance: instance,
                epoch: ProbeEpoch::new(epoch),
            },
            snapshot: run.document.baseline.snapshot.clone(),
        }
    }

    /// Complete attestation from a synthetic trusted host, independent of simulation result types.
    fn admission(
        run: &ExecutionRun,
        action: ActionId,
        instance: Uuid,
        epoch: u64,
    ) -> AdmissionReport {
        let contract = &run.plan.actions()[action.get() as usize].informational_contract;
        AdmissionReport {
            run_id: run.run_id(),
            action,
            observation: observation(run, instance, epoch),
            applicability: LiveCheckStatus::Satisfied,
            held_resources: contract.affected_resources.clone(),
            checks: contract
                .prerequisites
                .iter()
                .cloned()
                .map(|condition| LiveCheck {
                    condition,
                    status: LiveCheckStatus::Satisfied,
                })
                .collect(),
        }
    }

    /// Classified synthetic backend completion, always carrying a separately supplied observation.
    fn completion(
        run: &ExecutionRun,
        action: ActionId,
        instance: Uuid,
        epoch: u64,
        outcome: BackendOutcome,
    ) -> BackendCompletion {
        BackendCompletion {
            run_id: run.run_id(),
            action,
            outcome,
            observation: observation(run, instance, epoch),
        }
    }

    /// Complete synthetic postcondition report bound to the retained backend observation.
    fn verification(run: &ExecutionRun, action: ActionId) -> VerificationReport {
        let contract = &run.plan.actions()[action.get() as usize].informational_contract;
        let observed = run
            .completion(action)
            .unwrap_or_else(|error| panic!("completion lookup: {error}"))
            .unwrap_or_else(|| panic!("missing completion"));
        VerificationReport {
            run_id: run.run_id(),
            action,
            observation: observed.observation.reference,
            checks: contract
                .postconditions
                .iter()
                .cloned()
                .map(|condition| LiveCheck {
                    condition,
                    status: LiveCheckStatus::Satisfied,
                })
                .collect(),
        }
    }

    /// Full final attestation from a synthetic host, including protected-area preservation.
    fn goal_report(run: &ExecutionRun, instance: Uuid, epoch: u64) -> GoalVerificationReport {
        GoalVerificationReport {
            run_id: run.run_id(),
            observation: observation(run, instance, epoch),
            checks: run
                .goal_aspects
                .iter()
                .cloned()
                .map(|condition| LiveCheck {
                    condition,
                    status: LiveCheckStatus::Satisfied,
                })
                .collect(),
            preservation: LiveCheckStatus::Satisfied,
        }
    }

    /// Runs one synthetic action through the explicit lifecycle without any backend dispatcher.
    fn verify_one(run: &mut ExecutionRun, action: ActionId, instance: Uuid, epoch: u64) {
        run.begin_check(action)
            .unwrap_or_else(|error| panic!("begin admission: {error}"));
        let report = admission(run, action, instance, epoch);
        assert!(
            run.record_admission(report)
                .unwrap_or_else(|error| panic!("record admission: {error}"))
        );
        let report = completion(run, action, instance, epoch + 1, BackendOutcome::Succeeded);
        run.record_completion(report)
            .unwrap_or_else(|error| panic!("record completion: {error}"));
        let report = verification(run, action);
        assert!(
            run.record_verification(report)
                .unwrap_or_else(|error| panic!("record verification: {error}"))
        );
    }

    /// Dependencies require verified postconditions; backend success and progress are insufficient.
    #[test]
    fn successors_wait_for_verified_results_and_events_replay_in_order() {
        let document = crate::wire::tests::document_fixture();
        let baseline = document.clone();
        let mut run =
            ExecutionRun::new(&document).unwrap_or_else(|error| panic!("create run: {error}"));
        let other = ExecutionRun::new(&document)
            .unwrap_or_else(|error| panic!("create another attempt: {error}"));
        assert_ne!(run.run_id(), other.run_id());
        assert_ne!(run.run_id(), run.context().plan_id);
        let action = ActionId::new(0);
        let successor = ActionId::new(1);
        let instance = Uuid::now_v7();
        run.begin_check(action)
            .unwrap_or_else(|error| panic!("check first action: {error}"));
        let report = admission(&run, action, instance, 1);
        assert!(
            run.record_admission(report)
                .unwrap_or_else(|error| panic!("admit action: {error}"))
        );
        assert!(matches!(
            run.begin_check(successor),
            Err(ExecutionRunError::PredecessorNotVerified { .. })
        ));
        let observed = run.latest_observation().cloned();
        run.record_progress(action, "backend in progress".to_owned())
            .unwrap_or_else(|error| panic!("record progress: {error}"));
        assert_eq!(run.latest_observation(), observed.as_ref());
        assert_eq!(
            run.state(action)
                .unwrap_or_else(|error| panic!("read state: {error}")),
            ActionRunState::Running
        );
        let report = completion(&run, action, instance, 2, BackendOutcome::Succeeded);
        run.record_completion(report)
            .unwrap_or_else(|error| panic!("backend completion: {error}"));
        assert!(matches!(
            run.begin_check(successor),
            Err(ExecutionRunError::PredecessorNotVerified { .. })
        ));
        assert_eq!(
            run.state(action)
                .unwrap_or_else(|error| panic!("read state: {error}")),
            ActionRunState::Verifying
        );
        let report = verification(&run, action);
        assert!(
            run.record_verification(report)
                .unwrap_or_else(|error| panic!("verify first action: {error}"))
        );
        run.begin_check(successor)
            .unwrap_or_else(|error| panic!("verified predecessor permits checking: {error}"));
        assert!(!run.is_verified());
        let events = run.events_since(0).cloned().collect::<Vec<_>>();
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.run_id, run.run_id());
            assert_eq!(event.sequence, index as u64 + 1);
        }
        assert_eq!(
            run.events_since(3).cloned().collect::<Vec<_>>(),
            events[3..]
        );
        let json = serde_json::to_string(&events)
            .unwrap_or_else(|error| panic!("serialize progress: {error}"));
        assert_eq!(
            serde_json::from_str::<Vec<RunEvent>>(&json)
                .unwrap_or_else(|error| panic!("restore progress: {error}")),
            events
        );
        assert_eq!(document, baseline);
        assert_eq!(run.context().historical_baseline, &baseline.baseline);
    }

    /// Unknown admission never invokes an attempt; a known violation dominates unknown evidence.
    #[test]
    fn admission_requires_all_live_evidence_and_known_violation_stops_the_run() {
        let document = crate::wire::tests::document_fixture();
        let mut run =
            ExecutionRun::new(&document).unwrap_or_else(|error| panic!("create run: {error}"));
        let action = ActionId::new(0);
        let instance = Uuid::now_v7();
        run.begin_check(action)
            .unwrap_or_else(|error| panic!("begin check: {error}"));
        let mut report = admission(&run, action, instance, 1);
        report.applicability = LiveCheckStatus::Unknown {
            required_evidence: "fresh target identity".to_owned(),
        };
        assert!(
            !run.record_admission(report)
                .unwrap_or_else(|error| panic!("retain deferred admission: {error}"))
        );
        assert_eq!(
            run.state(action)
                .unwrap_or_else(|error| panic!("read state: {error}")),
            ActionRunState::Checking
        );
        assert!(matches!(
            run.record_completion(completion(
                &run,
                action,
                instance,
                2,
                BackendOutcome::Succeeded
            )),
            Err(ExecutionRunError::InvalidTransition { .. })
        ));
        let mut report = admission(&run, action, instance, 2);
        report.checks[0].status = LiveCheckStatus::Unknown {
            required_evidence: "provider evidence".to_owned(),
        };
        report.checks[1].status = LiveCheckStatus::Violated {
            reason: "target kind changed".to_owned(),
        };
        assert!(
            !run.record_admission(report)
                .unwrap_or_else(|error| panic!("retain failed admission: {error}"))
        );
        assert_eq!(
            run.state(action)
                .unwrap_or_else(|error| panic!("read state: {error}")),
            ActionRunState::Failed
        );
        assert!(matches!(
            run.begin_check(ActionId::new(1)),
            Err(ExecutionRunError::RunStopped)
        ));
        assert!(
            run.admission(action)
                .unwrap_or_else(|error| panic!("read admission: {error}"))
                .is_some()
        );
    }

    /// Malformed proofs, stale generations, and wrong attempt IDs cannot mutate run state.
    #[test]
    fn admission_rejects_missing_checks_leases_and_wrong_observation_binding() {
        let document = crate::wire::tests::document_fixture();
        let action = ActionId::new(0);
        let instance = Uuid::now_v7();
        for case in 0..7 {
            let mut run =
                ExecutionRun::new(&document).unwrap_or_else(|error| panic!("create run: {error}"));
            run.begin_check(action)
                .unwrap_or_else(|error| panic!("begin check: {error}"));
            let mut report = admission(&run, action, instance, 1);
            match case {
                0 => {
                    report.checks.pop();
                }
                1 => report.checks.push(report.checks[0].clone()),
                2 => report.checks.push(LiveCheck {
                    condition: ActionPrerequisite::Writable(NodeId::new()),
                    status: LiveCheckStatus::Satisfied,
                }),
                3 => report.held_resources.clear(),
                4 => {
                    for resource in &mut report.held_resources {
                        resource.mode = ResourceAccessMode::Read;
                    }
                }
                5 => report.run_id = Uuid::now_v7(),
                _ => report.observation.reference.probe_instance = Uuid::nil(),
            }
            assert!(run.record_admission(report).is_err());
            assert_eq!(
                run.state(action)
                    .unwrap_or_else(|error| panic!("read state: {error}")),
                ActionRunState::Checking
            );
            assert!(run.latest_observation().is_none());
            assert!(
                run.admission(action)
                    .unwrap_or_else(|error| panic!("read admission: {error}"))
                    .is_none()
            );
            assert_eq!(run.events_since(0).count(), 1);
        }
        let mut run =
            ExecutionRun::new(&document).unwrap_or_else(|error| panic!("create run: {error}"));
        run.begin_check(action)
            .unwrap_or_else(|error| panic!("begin check: {error}"));
        let mut report = admission(&run, action, instance, 1);
        report.applicability = LiveCheckStatus::Unknown {
            required_evidence: "identity".to_owned(),
        };
        run.record_admission(report)
            .unwrap_or_else(|error| panic!("defer admission: {error}"));
        assert!(matches!(
            run.record_admission(admission(&run, action, instance, 1)),
            Err(ExecutionRunError::StaleObservation)
        ));
        assert!(matches!(
            run.record_admission(admission(&run, action, Uuid::now_v7(), 2)),
            Err(ExecutionRunError::StaleObservation)
        ));
        assert!(matches!(
            run.state(ActionId::new(u32::MAX)),
            Err(ExecutionRunError::UnknownAction(_))
        ));
    }

    /// Backend failures retain actual partial state and never apply a predicted successful effect.
    #[test]
    fn failed_and_unknown_completion_publish_observation_and_forbid_retry() {
        let document = crate::wire::tests::document_fixture();
        for outcome in [
            BackendOutcome::Failed {
                reason: "partial failure".to_owned(),
            },
            BackendOutcome::OutcomeUnknown {
                reason: "interrupted".to_owned(),
            },
        ] {
            let mut run =
                ExecutionRun::new(&document).unwrap_or_else(|error| panic!("create run: {error}"));
            let action = ActionId::new(0);
            let instance = Uuid::now_v7();
            run.begin_check(action)
                .unwrap_or_else(|error| panic!("begin check: {error}"));
            run.record_admission(admission(&run, action, instance, 1))
                .unwrap_or_else(|error| panic!("admit action: {error}"));
            let mut report = completion(&run, action, instance, 2, outcome.clone());
            report.observation.snapshot.graph = storage_core::model::NodeGraph::new();
            let actual = report.observation.clone();
            run.record_completion(report)
                .unwrap_or_else(|error| panic!("retain partial outcome: {error}"));
            assert_eq!(run.latest_observation(), Some(&actual));
            assert_ne!(actual.snapshot.graph, document.requested_goal.graph);
            assert!(matches!(
                run.begin_check(action),
                Err(ExecutionRunError::InvalidTransition { .. })
            ));
            assert!(matches!(
                run.begin_check(ActionId::new(1)),
                Err(ExecutionRunError::RunStopped)
            ));
            assert_eq!(
                run.completion(action)
                    .unwrap_or_else(|error| panic!("read completion: {error}"))
                    .map(|completion| &completion.outcome),
                Some(&outcome)
            );
        }
        let mut run =
            ExecutionRun::new(&document).unwrap_or_else(|error| panic!("create run: {error}"));
        let action = ActionId::new(0);
        let instance = Uuid::now_v7();
        run.begin_check(action)
            .unwrap_or_else(|error| panic!("begin check: {error}"));
        run.record_admission(admission(&run, action, instance, 1))
            .unwrap_or_else(|error| panic!("admit action: {error}"));
        let actual = run.latest_observation().cloned();
        assert!(matches!(
            run.record_completion(completion(
                &run,
                action,
                instance,
                1,
                BackendOutcome::Succeeded
            )),
            Err(ExecutionRunError::StaleObservation)
        ));
        run.mark_outcome_unknown(action, "post-backend probe unavailable".to_owned())
            .unwrap_or_else(|error| panic!("stop unknown invocation: {error}"));
        assert_eq!(run.latest_observation(), actual.as_ref());
        assert_eq!(
            run.unknown_reason(action)
                .unwrap_or_else(|error| panic!("read unknown reason: {error}")),
            Some("post-backend probe unavailable")
        );
    }

    /// Generic success, missing postconditions, and stale verification cannot grant Verified.
    #[test]
    fn verification_requires_exact_complete_postconditions_and_actual_observation() {
        let document = crate::wire::tests::document_fixture();
        let action = ActionId::new(0);
        for case in 0..4 {
            let mut run =
                ExecutionRun::new(&document).unwrap_or_else(|error| panic!("create run: {error}"));
            let instance = Uuid::now_v7();
            run.begin_check(action)
                .unwrap_or_else(|error| panic!("begin check: {error}"));
            run.record_admission(admission(&run, action, instance, 1))
                .unwrap_or_else(|error| panic!("admit action: {error}"));
            run.record_completion(completion(
                &run,
                action,
                instance,
                2,
                BackendOutcome::Succeeded,
            ))
            .unwrap_or_else(|error| panic!("record backend result: {error}"));
            let mut report = verification(&run, action);
            match case {
                0 => report.checks.clear(),
                1 => {
                    report.checks = vec![LiveCheck {
                        condition: Postcondition::NodeReprobed(NodeId::new()),
                        status: LiveCheckStatus::Satisfied,
                    }]
                }
                2 => report.observation.epoch = ProbeEpoch::new(1),
                _ => report.run_id = Uuid::now_v7(),
            }
            assert!(run.record_verification(report).is_err());
            assert_eq!(
                run.state(action)
                    .unwrap_or_else(|error| panic!("read state: {error}")),
                ActionRunState::Verifying
            );
            let mut report = verification(&run, action);
            report.checks[0].status = LiveCheckStatus::Unknown {
                required_evidence: "node identity".to_owned(),
            };
            assert!(
                !run.record_verification(report)
                    .unwrap_or_else(|error| panic!("retain unknown verification: {error}"))
            );
            assert_eq!(
                run.state(action)
                    .unwrap_or_else(|error| panic!("read state: {error}")),
                ActionRunState::OutcomeUnknown
            );
            assert!(matches!(
                run.begin_check(ActionId::new(1)),
                Err(ExecutionRunError::RunStopped)
            ));
        }
    }

    /// Final goal verification is separate, complete, fresh, and bound to a frozen description.
    #[test]
    fn all_verified_actions_do_not_automatically_verify_the_actual_goal() {
        let mut document = crate::wire::tests::document_fixture();
        let original = document.clone();
        let mut run =
            ExecutionRun::new(&document).unwrap_or_else(|error| panic!("create run: {error}"));
        let instance = Uuid::now_v7();
        assert!(matches!(
            run.record_goal_verification(goal_report(&run, instance, 1)),
            Err(ExecutionRunError::ActionsNotVerified)
        ));
        document.plan_id = Uuid::now_v7();
        document
            .requested_goal
            .intents
            .push(storage_core::model::OperationIntent::Check {
                target: NodeId::new(),
                force: true,
            });
        for id in 0..4 {
            verify_one(&mut run, ActionId::new(id), instance, u64::from(id) * 2 + 1);
        }
        assert!(!run.is_verified());
        assert_eq!(run.context().plan_id, original.plan_id);
        assert_eq!(run.context().requested_goal, &original.requested_goal);
        let mut report = goal_report(&run, instance, 9);
        report.checks.pop();
        assert!(matches!(
            run.record_goal_verification(report),
            Err(ExecutionRunError::IncompleteChecks)
        ));
        let mut report = goal_report(&run, instance, 9);
        report.preservation = LiveCheckStatus::Unknown {
            required_evidence: "protected objects".to_owned(),
        };
        assert!(
            !run.record_goal_verification(report)
                .unwrap_or_else(|error| panic!("record conditional final goal: {error}"))
        );
        assert!(!run.is_verified());
        assert!(run.goal_verification().is_some());
        assert!(
            run.record_goal_verification(goal_report(&run, instance, 10))
                .unwrap_or_else(|error| panic!("verify actual final goal: {error}"))
        );
        assert!(run.is_verified());
    }

    /// Builds two existing-filesystem checks with no resource-write conflict or dependencies.
    fn inspection_document(force: bool) -> PlanDocument {
        use storage_provider::{
            ActionDescriptionContext, FilesystemAction, ProviderAction, describe_action,
        };
        let mut document = crate::wire::tests::document_fixture();
        let filesystem = document
            .requested_goal
            .graph
            .nodes()
            .find_map(|(id, node)| {
                matches!(
                    node.kind.kind,
                    storage_core::model::NodeKind::Filesystem { .. }
                )
                .then_some(*id)
            })
            .unwrap_or_else(|| panic!("filesystem fixture"));
        document.baseline.snapshot.graph = document.requested_goal.graph.clone();
        document.requested_goal.mounts.runtime.clear();
        document.actions.truncate(2);
        document.dependencies.clear();
        for action in &mut document.actions {
            action.operation =
                ProviderAction::Filesystem(FilesystemAction::Check { filesystem, force });
            action.informational_contract = describe_action(
                &action.operation,
                ActionDescriptionContext {
                    graph: &document.baseline.snapshot.graph,
                },
            );
        }
        document
    }

    /// A completed check and NodeReprobed cannot stand in for a verified clean-check result.
    #[test]
    fn forced_check_requires_actual_clean_result_before_verified() {
        let document = inspection_document(true);
        let mut run = ExecutionRun::new(&document)
            .unwrap_or_else(|error| panic!("create clean-check run: {error}"));
        let action = ActionId::new(0);
        let instance = Uuid::now_v7();
        run.begin_check(action)
            .unwrap_or_else(|error| panic!("begin clean check: {error}"));
        run.record_admission(admission(&run, action, instance, 1))
            .unwrap_or_else(|error| panic!("admit check: {error}"));
        run.record_completion(completion(
            &run,
            action,
            instance,
            2,
            BackendOutcome::Succeeded,
        ))
        .unwrap_or_else(|error| panic!("checker completed: {error}"));
        let mut report = verification(&run, action);
        report
            .checks
            .retain(|check| !matches!(check.condition, Postcondition::FilesystemCheckPassed(_)));
        assert!(matches!(
            run.record_verification(report),
            Err(ExecutionRunError::IncompleteChecks)
        ));
        let mut report = verification(&run, action);
        for check in &mut report.checks {
            if matches!(check.condition, Postcondition::FilesystemCheckPassed(_)) {
                check.status = LiveCheckStatus::Violated {
                    reason: "checker found errors".to_owned(),
                };
            }
        }
        assert!(
            !run.record_verification(report)
                .unwrap_or_else(|error| panic!("retain checker errors: {error}"))
        );
        assert_eq!(
            run.state(action)
                .unwrap_or_else(|error| panic!("read check state: {error}")),
            ActionRunState::Failed
        );
        assert!(matches!(
            run.begin_check(ActionId::new(1)),
            Err(ExecutionRunError::RunStopped)
        ));
    }

    /// Independent reads are serial in this minimal runtime, despite a valid parallel DAG.
    #[test]
    fn only_one_action_may_hold_the_active_runtime_phase() {
        let document = inspection_document(false);
        let mut run = ExecutionRun::new(&document)
            .unwrap_or_else(|error| panic!("create independent-check run: {error}"));
        run.begin_check(ActionId::new(0))
            .unwrap_or_else(|error| panic!("begin first check: {error}"));
        assert!(matches!(
            run.begin_check(ActionId::new(1)),
            Err(ExecutionRunError::ActiveAction)
        ));
    }
}
