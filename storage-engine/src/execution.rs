//! Execution lifecycle and trusted live-check contracts; no backend dispatch is implemented.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use storage_core::model::{CurrentState, DesiredState, ProbeEpoch};
use storage_provider::{ActionPrerequisite, Postcondition, ResourceAccess, ResourceAccessMode};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    ActionId, ExecutionPlan, GoalAspect, HistoricalBaseline, PlanDocument, PlanDocumentError,
    PlannedAction, ProtectedScope,
};

/// Maximum number of retained events per live run; older events are evicted, never persisted.
pub const MAX_RUN_EVENTS: usize = 1024;
/// Byte bound on one provider progress message, limiting the retained event payload as well as count.
pub const MAX_PROGRESS_BYTES: usize = 4096;

/// One action's runtime lifecycle, never stored in the immutable plan document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionRunState {
    /// No attempt has begun.
    Waiting,
    /// Live evidence is being collected before any invocation attempt.
    Checking,
    /// Live admission passed; the host has not yet crossed the checkpoint/dispatch boundary.
    Admitted,
    /// The host marked dispatch started after durable checkpoint preparation; effects are unverified.
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
        #[serde(serialize_with = "storage_core::diagnostics::serialize_message")]
        reason: String,
    },
    /// Evidence is incomplete; simulation is not an acceptable substitute.
    Unknown {
        /// Evidence still needed from the runtime environment.
        #[serde(serialize_with = "storage_core::diagnostics::serialize_message")]
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
        #[serde(serialize_with = "storage_core::diagnostics::serialize_message")]
        reason: String,
    },
    /// Interruption or partial outcome requires observation and explicit recovery.
    OutcomeUnknown {
        /// Provider-owned explanation of the unknown result.
        #[serde(serialize_with = "storage_core::diagnostics::serialize_message")]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
        /// Provider-owned progress retained raw in Rust and omitted from JSON.
        #[serde(serialize_with = "storage_core::diagnostics::serialize_message")]
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

/// Bounded replay after an exclusive last-consumed cursor; a gap requires a fresh run snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsPage {
    /// All retained events newer than the requested cursor, in sequence order.
    pub events: Vec<RunEvent>,
    /// Last published sequence; pass this unchanged as `since` for the next page, initially zero.
    pub next_sequence: u64,
    /// True when at least one event newer than the requested cursor was already evicted.
    pub gap: bool,
}

/// Current action facts for reconnecting clients, independent of evicted event history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionStatus {
    /// Frozen global action identity.
    pub action: ActionId,
    /// Actual lifecycle state, never a projected effect.
    pub state: ActionRunState,
    /// Latest applicability decision; individual prerequisites remain independently visible.
    pub applicability: Option<LiveCheckStatus>,
    /// Publication that supplied the latest prerequisite and applicability evidence.
    pub admission_observation: Option<ObservationReference>,
    /// Independent post-backend publication, without cloning a complete graph for every action.
    pub completion_observation: Option<ObservationReference>,
    /// Complete most recently checked prerequisite set, empty before admission.
    pub prerequisites: Vec<LiveCheck<ActionPrerequisite>>,
    /// Classified backend result, absent until an actual completion was recorded.
    pub outcome: Option<BackendOutcome>,
    /// Complete postcondition checks, absent before verification.
    pub postconditions: Option<Vec<LiveCheck<Postcondition>>>,
    /// Checkpoint preparation or endpoint failure before dispatch.
    #[serde(serialize_with = "storage_core::diagnostics::serialize_optional_message")]
    pub pre_dispatch_failure: Option<String>,
    /// Why a running invocation could not be classified or observed.
    #[serde(serialize_with = "storage_core::diagnostics::serialize_optional_message")]
    pub unknown_reason: Option<String>,
}

/// Complete current execution view used after an event gap; this is not a resumable run document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunSnapshot {
    /// Live attempt identity, independent of its frozen plan.
    pub run_id: Uuid,
    /// Reviewed immutable plan identity.
    pub plan_id: Uuid,
    /// Every frozen action in global action order, including actions that have not started.
    pub actions: Vec<ActionStatus>,
    /// Terminal action outcome, exhausted event cursor, or a completed final verification report.
    /// Waiting admission with missing evidence remains unfinished; success is reported separately.
    pub finished: bool,
    /// True only after the independent actual-goal and preservation report passes.
    pub verified: bool,
    /// Last published event; use as the exclusive cursor after replacing state with this snapshot.
    pub event_sequence: u64,
    /// Latest accepted actual provider publication, never a simulation or historical fallback.
    pub observation: Option<LiveObservation>,
    /// Independent final evidence; action success alone cannot populate this field.
    pub goal_verification: Option<GoalVerificationReport>,
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
    /// Failure before dispatch, without claiming any backend invocation.
    pre_dispatch_failure: Option<String>,
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
    events: VecDeque<RunEvent>,
    /// Next event identity, independent of the number of retained entries.
    next_event_sequence: u64,
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
    #[error("another action is checking, admitted, running, or verifying")]
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
    /// Malformed actual state cannot replace the last accepted observation.
    #[error(transparent)]
    InvalidObservation(#[from] crate::live::ObservationError),
    /// The final goal cannot be verified before all actions have verified results.
    #[error("final verification requires every action to be Verified")]
    ActionsNotVerified,
    /// No transition may wrap the event cursor or partially publish a multi-event admission.
    #[error("run event sequence exhausted")]
    EventSequenceExhausted,
    /// A future cursor cannot acknowledge events that this run has never published.
    #[error("event cursor {requested} exceeds latest sequence {latest}")]
    InvalidEventCursor {
        /// Exclusive last-consumed sequence supplied by the client.
        requested: u64,
        /// Last event published by this run.
        latest: u64,
    },
    /// Provider progress cannot grow the bounded event buffer without a byte limit.
    #[error("progress exceeds {MAX_PROGRESS_BYTES} bytes")]
    ProgressTooLarge,
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
                pre_dispatch_failure: None,
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
            events: VecDeque::new(),
            next_event_sequence: 1,
        })
    }

    /// Returns this attempt's identity.
    pub const fn run_id(&self) -> Uuid {
        self.run_id
    }

    /// Returns the immutable reviewed description, never a mutable editing draft.
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

    /// Returns the host's reason for stopping an admitted action before dispatch.
    pub fn pre_dispatch_failure(
        &self,
        action: ActionId,
    ) -> Result<Option<&str>, ExecutionRunError> {
        Ok(self.action(action)?.pre_dispatch_failure.as_deref())
    }

    /// Returns why an invocation was left unclassified when a new probe could not be obtained.
    pub fn unknown_reason(&self, action: ActionId) -> Result<Option<&str>, ExecutionRunError> {
        Ok(self.action(action)?.unknown_reason.as_deref())
    }

    /// Copies current action/evidence state even if the corresponding events were already evicted.
    pub fn snapshot(&self) -> RunSnapshot {
        RunSnapshot {
            run_id: self.run_id,
            plan_id: self.document.plan_id,
            actions: self
                .plan
                .actions()
                .iter()
                .zip(&self.actions)
                .map(|(planned, action)| ActionStatus {
                    action: planned.id,
                    state: action.state,
                    applicability: action
                        .admission
                        .as_ref()
                        .map(|report| report.applicability.clone()),
                    admission_observation: action
                        .admission
                        .as_ref()
                        .map(|report| report.observation.reference),
                    completion_observation: action
                        .completion
                        .as_ref()
                        .map(|completion| completion.observation.reference),
                    prerequisites: action
                        .admission
                        .as_ref()
                        .map(|report| report.checks.clone())
                        .unwrap_or_default(),
                    outcome: action
                        .completion
                        .as_ref()
                        .map(|completion| completion.outcome.clone()),
                    postconditions: action
                        .verification
                        .as_ref()
                        .map(|report| report.checks.clone()),
                    pre_dispatch_failure: action.pre_dispatch_failure.clone(),
                    unknown_reason: action.unknown_reason.clone(),
                })
                .collect(),
            finished: self.goal_verification.is_some()
                || self.next_event_sequence == u64::MAX
                || self.actions.iter().any(|action| {
                    matches!(
                        action.state,
                        ActionRunState::Failed | ActionRunState::OutcomeUnknown
                    )
                }),
            verified: self.is_verified(),
            event_sequence: self.next_event_sequence - 1,
            observation: self.latest_observation.clone(),
            goal_verification: self.goal_verification.clone(),
        }
    }

    /// Returns bounded ordered replay and detects events lost since an exclusive cursor.
    /// Future cursors are rejected so a client cannot silently skip subsequent updates.
    pub fn events_page(&self, since: u64) -> Result<EventsPage, ExecutionRunError> {
        let latest = self.next_event_sequence - 1;
        if since > latest {
            return Err(ExecutionRunError::InvalidEventCursor {
                requested: since,
                latest,
            });
        }
        Ok(EventsPage {
            events: self.events_since(since).cloned().collect(),
            next_sequence: latest,
            gap: self
                .events
                .front()
                .is_some_and(|first| since < first.sequence - 1),
        })
    }

    /// Compatibility iterator over retained events; new clients use events_page to detect eviction gaps.
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
                ActionRunState::Checking
                    | ActionRunState::Admitted
                    | ActionRunState::Running
                    | ActionRunState::Verifying
            )
        }) {
            return Err(ExecutionRunError::ActiveAction);
        }
        self.set_state(action, ActionRunState::Checking)?;
        Ok(())
    }

    /// Records live evidence, entering `Admitted` only when every requirement is satisfied.
    /// Unknown evidence stays in `Checking`; a known violation stops the run before invocation.
    /// The host must prepare its checkpoint before marking dispatch started under retained leases.
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
        let state = evidence_state(statuses, ActionRunState::Admitted, ActionRunState::Checking);
        let admitted = state == ActionRunState::Admitted;
        self.ensure_event_capacity(if state == ActionRunState::Checking {
            1
        } else {
            2
        })?;
        self.latest_observation = Some(report.observation.clone());
        self.actions[action.get() as usize].admission = Some(report);
        self.push_event(RunEventKind::Admission { action, admitted })?;
        if state != ActionRunState::Checking {
            self.set_state(action, state)?;
        }
        Ok(admitted)
    }

    /// Marks the invocation boundary after the trusted host has durably armed its checkpoint.
    /// This transition neither creates a checkpoint nor proves that a backend was actually called.
    pub fn mark_dispatch_started(&mut self, action: ActionId) -> Result<(), ExecutionRunError> {
        self.require_state(action, ActionRunState::Admitted)?;
        self.set_state(action, ActionRunState::Running)?;
        Ok(())
    }

    /// Stops an admitted action when checkpoint preparation fails, before any backend invocation.
    pub fn mark_pre_dispatch_failed(
        &mut self,
        action: ActionId,
        reason: String,
    ) -> Result<(), ExecutionRunError> {
        self.require_state(action, ActionRunState::Admitted)?;
        self.ensure_event_capacity(1)?;
        self.actions[action.get() as usize].pre_dispatch_failure = Some(reason);
        self.set_state(action, ActionRunState::Failed)?;
        Ok(())
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
        self.ensure_event_capacity(1)?;
        self.latest_observation = Some(completion.observation.clone());
        self.actions[action.get() as usize].completion = Some(completion);
        self.set_state(action, state)?;
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
        self.ensure_event_capacity(1)?;
        self.actions[action.get() as usize].verification = Some(report);
        self.set_state(action, state)?;
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
        self.ensure_event_capacity(1)?;
        self.actions[action.get() as usize].unknown_reason = Some(reason);
        self.set_state(action, ActionRunState::OutcomeUnknown)?;
        Ok(())
    }

    /// Records provider progress without changing state or projecting an effect.
    pub fn record_progress(
        &mut self,
        action: ActionId,
        detail: String,
    ) -> Result<(), ExecutionRunError> {
        self.require_state(action, ActionRunState::Running)?;
        if detail.len() > MAX_PROGRESS_BYTES {
            return Err(ExecutionRunError::ProgressTooLarge);
        }
        self.push_event(RunEventKind::Progress { action, detail })?;
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
        self.ensure_event_capacity(1)?;
        self.latest_observation = Some(report.observation.clone());
        self.goal_verification = Some(report);
        self.push_event(RunEventKind::GoalVerification { verified })?;
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
        observation.validate()?;
        Ok(())
    }

    /// Rejects exhaustion before mutating evidence or publishing any part of a transition.
    fn ensure_event_capacity(&self, count: u64) -> Result<(), ExecutionRunError> {
        self.next_event_sequence
            .checked_add(count)
            .ok_or(ExecutionRunError::EventSequenceExhausted)?;
        Ok(())
    }

    /// Publishes one validated state change with a monotonically ordered event.
    fn set_state(
        &mut self,
        action: ActionId,
        state: ActionRunState,
    ) -> Result<(), ExecutionRunError> {
        self.ensure_event_capacity(1)?;
        self.actions[action.get() as usize].state = state;
        self.push_event(RunEventKind::ActionState { action, state })
    }

    /// Appends one bounded event without deriving its identity from the truncated buffer.
    fn push_event(&mut self, kind: RunEventKind) -> Result<(), ExecutionRunError> {
        self.ensure_event_capacity(1)?;
        if self.events.len() == MAX_RUN_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(RunEvent {
            run_id: self.run_id,
            sequence: self.next_event_sequence,
            kind,
        });
        self.next_event_sequence += 1;
        Ok(())
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
        run.mark_dispatch_started(action)
            .unwrap_or_else(|error| panic!("mark dispatch: {error}"));
        let report = completion(run, action, instance, epoch + 1, BackendOutcome::Succeeded);
        run.record_completion(report)
            .unwrap_or_else(|error| panic!("record completion: {error}"));
        let report = verification(run, action);
        assert!(
            run.record_verification(report)
                .unwrap_or_else(|error| panic!("record verification: {error}"))
        );
    }

    /// A snapshot recovers current evidence after replay gaps without declaring partial actions successful.
    #[test]
    fn snapshots_preserve_actual_state_and_unknown_outcomes_after_event_eviction()
    -> Result<(), ExecutionRunError> {
        let mut run = ExecutionRun::new(&crate::wire::tests::document_fixture())?;
        let initial = run.snapshot();
        assert!(!initial.finished && !initial.verified);
        assert!(initial.observation.is_none());
        assert_eq!(initial.event_sequence, 0);
        let action = ActionId::new(0);
        let instance = Uuid::now_v7();
        run.begin_check(action)?;
        let report = admission(&run, action, instance, 1);
        run.record_admission(report)?;
        run.mark_dispatch_started(action)?;
        for _ in 0..MAX_RUN_EVENTS {
            run.record_progress(action, "native progress".into())?;
        }
        assert!(run.events_page(0)?.gap);
        let current = run.snapshot();
        assert!(!current.finished && !current.verified);
        assert_eq!(current.actions[0].state, ActionRunState::Running);
        assert_eq!(
            current.actions[0].applicability,
            Some(LiveCheckStatus::Satisfied)
        );
        assert!(current.actions[0].outcome.is_none());
        assert!(run.events_page(current.event_sequence)?.events.is_empty());
        let completion = completion(
            &run,
            action,
            instance,
            2,
            BackendOutcome::OutcomeUnknown {
                reason: "lost native completion".into(),
            },
        );
        run.record_completion(completion)?;
        let stopped = run.snapshot();
        assert!(stopped.finished && !stopped.verified);
        assert_eq!(stopped.actions[0].state, ActionRunState::OutcomeUnknown);
        assert!(stopped.goal_verification.is_none());
        assert_eq!(
            stopped
                .observation
                .as_ref()
                .map(|value| value.reference.epoch.get()),
            Some(2)
        );
        assert!(serde_json::to_string(&stopped).is_ok());
        Ok(())
    }

    /// Exclusive cursors report eviction gaps, reject future acknowledgments and resume without duplicates.
    #[test]
    fn event_pages_detect_gaps_at_the_exact_eviction_boundary() -> Result<(), ExecutionRunError> {
        let mut run = ExecutionRun::new(&crate::wire::tests::document_fixture())?;
        assert_eq!(
            run.events_page(0)?,
            EventsPage {
                events: vec![],
                next_sequence: 0,
                gap: false
            }
        );
        assert!(matches!(
            run.events_page(1),
            Err(ExecutionRunError::InvalidEventCursor { .. })
        ));
        for _ in 0..MAX_RUN_EVENTS + 3 {
            run.push_event(RunEventKind::Progress {
                action: ActionId::new(0),
                detail: "observed progress".into(),
            })?;
        }
        let page = run.events_page(0)?;
        assert!(page.gap);
        assert_eq!(page.events.len(), MAX_RUN_EVENTS);
        let first = page.events[0].sequence;
        assert!(!run.events_page(first - 1)?.gap);
        assert!(run.events_page(first - 2)?.gap);
        let next = run.events_page(page.next_sequence)?;
        assert!(!next.gap);
        assert!(next.events.is_empty());
        assert_eq!(next.next_sequence, page.next_sequence);
        run.push_event(RunEventKind::GoalVerification { verified: false })?;
        assert_eq!(run.events_page(page.next_sequence)?.events.len(), 1);
        assert!(serde_json::to_string(&page).is_ok());
        Ok(())
    }

    /// Event identities survive eviction, oversized progress is rejected, and exhaustion is atomic.
    #[test]
    fn bounded_events_keep_monotonic_sequences_and_reject_exhaustion()
    -> Result<(), ExecutionRunError> {
        let mut run = ExecutionRun::new(&crate::wire::tests::document_fixture())?;
        let action = ActionId::new(0);
        let instance = Uuid::now_v7();
        run.begin_check(action)?;
        let report = admission(&run, action, instance, 1);
        run.record_admission(report)?;
        run.mark_dispatch_started(action)?;
        for _ in 0..MAX_RUN_EVENTS + 5 {
            run.record_progress(action, "actual progress".into())?;
        }
        assert_eq!(run.events.len(), MAX_RUN_EVENTS);
        assert!(run.events[0].sequence > 1);
        assert!(
            run.events
                .iter()
                .zip(run.events.iter().skip(1))
                .all(|(a, b)| b.sequence == a.sequence + 1)
        );
        let before = run.events.clone();
        assert!(matches!(
            run.record_progress(action, "я".repeat(MAX_PROGRESS_BYTES)),
            Err(ExecutionRunError::ProgressTooLarge)
        ));
        assert_eq!(run.events, before);
        run.next_event_sequence = u64::MAX;
        assert!(matches!(
            run.mark_outcome_unknown(action, "late callback".into()),
            Err(ExecutionRunError::EventSequenceExhausted)
        ));
        assert_eq!(run.state(action)?, ActionRunState::Running);
        assert!(run.unknown_reason(action)?.is_none());
        assert_eq!(run.events, before);
        let mut run = ExecutionRun::new(&crate::wire::tests::document_fixture())?;
        run.begin_check(action)?;
        run.next_event_sequence = u64::MAX - 1;
        let report = admission(&run, action, instance, 1);
        assert!(matches!(
            run.record_admission(report),
            Err(ExecutionRunError::EventSequenceExhausted)
        ));
        assert!(run.admission(action)?.is_none());
        assert!(run.latest_observation().is_none());
        assert_eq!(run.state(action)?, ActionRunState::Checking);
        Ok(())
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
        run.mark_dispatch_started(action)
            .unwrap_or_else(|error| panic!("mark dispatch: {error}"));
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
        let decoded = serde_json::from_str::<Vec<RunEvent>>(&json)
            .unwrap_or_else(|error| panic!("decode public progress: {error}"));
        assert_eq!(decoded.len(), events.len());
        for (public, raw) in decoded.iter().zip(&events) {
            assert_eq!((public.run_id, public.sequence), (raw.run_id, raw.sequence));
            if let RunEventKind::Progress { action, detail } = &public.kind {
                assert_eq!(*action, ActionId::new(0));
                assert_eq!(detail, storage_core::diagnostics::REDACTED);
                assert!(
                    matches!(&raw.kind, RunEventKind::Progress { detail, .. } if detail == "backend in progress")
                );
            } else {
                assert_eq!(public, raw);
            }
        }
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
        for case in 0..10 {
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
                6 => report.observation.reference.probe_instance = Uuid::nil(),
                7 => {
                    report.observation.snapshot.graph.insert_dependency(
                        storage_core::model::Dependency {
                            from: NodeId::new(),
                            to: NodeId::new(),
                            kind: storage_core::model::DependencyKind::Backs,
                        },
                    );
                }
                8 => {
                    let (id, mut node) = report
                        .observation
                        .snapshot
                        .graph
                        .nodes()
                        .next()
                        .map(|(id, node)| (*id, node.clone()))
                        .unwrap_or_else(|| panic!("fixture has an observed node"));
                    node.size.observed_in = Some(ProbeEpoch::new(99));
                    report
                        .observation
                        .snapshot
                        .graph
                        .replace_existing(id, node)
                        .unwrap_or_else(|error| panic!("replace fixture node: {error}"));
                }
                _ => report.observation.snapshot.mounts.entries.push(
                    storage_core::model::ObservedMount {
                        source: storage_core::model::MountSource::Filesystem(NodeId::new()),
                        target: "/dangling".into(),
                        options: Vec::new(),
                        context: storage_core::model::MountContext::Host,
                    },
                ),
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

    /// Completion and final checks reject malformed observations without dropping retained evidence.
    #[test]
    fn malformed_completion_and_goal_observations_leave_run_unchanged()
    -> Result<(), Box<dyn std::error::Error>> {
        let document = crate::wire::tests::document_fixture();
        let instance = Uuid::now_v7();
        let action = ActionId::new(0);
        let mut run = ExecutionRun::new(&document)?;
        run.begin_check(action)?;
        run.record_admission(admission(&run, action, instance, 1))?;
        run.mark_dispatch_started(action)?;
        let mut report = completion(&run, action, instance, 2, BackendOutcome::Succeeded);
        report
            .observation
            .snapshot
            .graph
            .insert_dependency(storage_core::model::Dependency {
                from: NodeId::new(),
                to: NodeId::new(),
                kind: storage_core::model::DependencyKind::Backs,
            });
        let before = run.latest_observation().cloned();
        let events = run.events_since(0).count();
        assert!(matches!(
            run.record_completion(report),
            Err(ExecutionRunError::InvalidObservation(_))
        ));
        assert_eq!(run.state(action)?, ActionRunState::Running);
        assert_eq!(run.latest_observation(), before.as_ref());
        assert!(run.completion(action)?.is_none());
        assert_eq!(run.events_since(0).count(), events);

        let mut run = ExecutionRun::new(&document)?;
        for id in 0..4 {
            verify_one(&mut run, ActionId::new(id), instance, u64::from(id) * 2 + 1);
        }
        let mut report = goal_report(&run, instance, 9);
        report
            .observation
            .snapshot
            .graph
            .insert_dependency(storage_core::model::Dependency {
                from: NodeId::new(),
                to: NodeId::new(),
                kind: storage_core::model::DependencyKind::Backs,
            });
        let before = run.latest_observation().cloned();
        let events = run.events_since(0).count();
        assert!(matches!(
            run.record_goal_verification(report),
            Err(ExecutionRunError::InvalidObservation(_))
        ));
        assert_eq!(run.latest_observation(), before.as_ref());
        assert!(run.goal_verification().is_none());
        assert!(!run.is_verified());
        assert_eq!(run.events_since(0).count(), events);
        Ok(())
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
            run.mark_dispatch_started(action)
                .unwrap_or_else(|error| panic!("mark dispatch: {error}"));
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
        run.mark_dispatch_started(action)
            .unwrap_or_else(|error| panic!("mark dispatch: {error}"));
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
            run.mark_dispatch_started(action)
                .unwrap_or_else(|error| panic!("mark dispatch: {error}"));
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
        run.mark_dispatch_started(action)
            .unwrap_or_else(|error| panic!("mark dispatch: {error}"));
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
        let action = ActionId::new(0);
        run.record_admission(admission(&run, action, Uuid::now_v7(), 1))
            .unwrap_or_else(|error| panic!("admit first action: {error}"));
        assert_eq!(run.state(action).ok(), Some(ActionRunState::Admitted));
        assert!(matches!(
            run.begin_check(ActionId::new(1)),
            Err(ExecutionRunError::ActiveAction)
        ));
    }

    /// Admission alone cannot accept progress/completion, and checkpoint failure stops successors.
    #[test]
    fn admitted_action_requires_dispatch_or_pre_dispatch_failure()
    -> Result<(), Box<dyn std::error::Error>> {
        let document = inspection_document(false);
        let action = ActionId::new(0);
        let instance = Uuid::now_v7();
        let mut run = ExecutionRun::new(&document)?;
        for state in [ActionRunState::Waiting, ActionRunState::Checking] {
            let events = run.events_since(0).count();
            assert!(run.mark_dispatch_started(action).is_err());
            assert!(
                run.mark_pre_dispatch_failed(action, "checkpoint error".into())
                    .is_err()
            );
            assert_eq!(run.state(action)?, state);
            assert_eq!(run.events_since(0).count(), events);
            if state == ActionRunState::Waiting {
                run.begin_check(action)?;
            }
        }
        assert!(run.record_admission(admission(&run, action, instance, 1))?);
        let events = run.events_since(0).count();
        assert_eq!(run.state(action)?, ActionRunState::Admitted);
        assert!(run.record_progress(action, "not running".into()).is_err());
        assert!(
            run.record_completion(completion(
                &run,
                action,
                instance,
                2,
                BackendOutcome::Succeeded
            ))
            .is_err()
        );
        assert!(
            run.mark_outcome_unknown(action, "not dispatched".into())
                .is_err()
        );
        assert_eq!(run.events_since(0).count(), events);
        let retained = run.latest_observation().cloned();
        run.mark_pre_dispatch_failed(action, "checkpoint fsync failed".into())?;
        assert_eq!(run.state(action)?, ActionRunState::Failed);
        assert_eq!(
            run.pre_dispatch_failure(action)?,
            Some("checkpoint fsync failed")
        );
        assert_eq!(run.latest_observation(), retained.as_ref());
        assert!(run.completion(action)?.is_none());
        assert!(run.unknown_reason(action)?.is_none());
        assert!(matches!(
            run.begin_check(ActionId::new(1)),
            Err(ExecutionRunError::RunStopped)
        ));
        assert!(run.mark_dispatch_started(action).is_err());

        let mut run = ExecutionRun::new(&document)?;
        run.begin_check(action)?;
        run.record_admission(admission(&run, action, instance, 1))?;
        run.mark_dispatch_started(action)?;
        assert!(run.mark_dispatch_started(action).is_err());
        assert!(
            run.mark_pre_dispatch_failed(action, "too late".into())
                .is_err()
        );
        assert_eq!(run.state(action)?, ActionRunState::Running);
        let events: Vec<_> = run.events_since(0).cloned().collect();
        assert!(events.iter().any(|event| matches!(
            event.kind,
            RunEventKind::ActionState {
                state: ActionRunState::Admitted,
                ..
            }
        )));
        assert_eq!(
            serde_json::from_str::<Vec<RunEvent>>(&serde_json::to_string(&events)?)?,
            events
        );
        assert_eq!(
            serde_json::to_string(&ActionRunState::Admitted)?,
            "\"admitted\""
        );
        Ok(())
    }
}

#[cfg(test)]
mod diagnostic_serialization {
    use super::*;
    use storage_core::diagnostics::REDACTED;

    /// JSON preserves lifecycle and cursors while every opaque execution explanation stays in Rust.
    #[test]
    fn execution_json_omits_free_text_without_mutating_retained_evidence()
    -> Result<(), serde_json::Error> {
        let raw = "SECRET token=abc\n/private/backend-path";
        let status = LiveCheckStatus::Unknown {
            required_evidence: raw.into(),
        };
        let outcome = BackendOutcome::Failed { reason: raw.into() };
        let action = ActionStatus {
            action: ActionId::new(0),
            state: ActionRunState::OutcomeUnknown,
            applicability: Some(status.clone()),
            admission_observation: None,
            completion_observation: None,
            prerequisites: vec![],
            outcome: Some(outcome.clone()),
            postconditions: None,
            pre_dispatch_failure: Some(raw.into()),
            unknown_reason: Some(raw.into()),
        };
        let event = RunEvent {
            run_id: Uuid::now_v7(),
            sequence: 7,
            kind: RunEventKind::Progress {
                action: ActionId::new(0),
                detail: raw.into(),
            },
        };
        let json = serde_json::to_value(&action)?;
        assert_eq!(json["state"], "outcome_unknown");
        assert_eq!(
            json["applicability"]["unknown"]["required_evidence"],
            REDACTED
        );
        assert_eq!(json["outcome"]["failed"]["reason"], REDACTED);
        assert_eq!(json["pre_dispatch_failure"], REDACTED);
        assert_eq!(json["unknown_reason"], REDACTED);
        assert!(!json.to_string().contains("SECRET"));
        let json = serde_json::to_value(&event)?;
        assert_eq!(json["sequence"], 7);
        assert_eq!(json["run_id"], event.run_id.to_string());
        assert_eq!(json["kind"]["progress"]["detail"], REDACTED);
        assert_eq!(action.outcome, Some(outcome));
        assert_eq!(action.applicability, Some(status));
        assert_eq!(action.unknown_reason.as_deref(), Some(raw));
        assert!(matches!(&event.kind, RunEventKind::Progress { detail, .. } if detail == raw));
        for value in [
            serde_json::to_value(LiveCheckStatus::Violated { reason: raw.into() })?,
            serde_json::to_value(BackendOutcome::OutcomeUnknown { reason: raw.into() })?,
        ] {
            assert!(!value.to_string().contains("SECRET"));
            assert!(value.to_string().contains(REDACTED));
        }
        Ok(())
    }
}
