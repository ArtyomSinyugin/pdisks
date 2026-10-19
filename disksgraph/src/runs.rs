//! Synchronous application execution; all effects still belong exclusively to the runtime coordinator.

use crate::*;
use storage_engine::{ExecutionRun, editing::OperationActions};
use storage_runtime::coordinator::ExecutionCoordinator;

/// Complete application view after the blocking attempt returns, including failures without events.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunSnapshot {
    /// Actual engine state; finished means this synchronous call ended, separately from verified success.
    #[serde(flatten)]
    pub execution: storage_engine::RunSnapshot,
    /// Trusted editing ownership, absent for every separately imported PlanDocument.
    pub source_map: Vec<OperationActions>,
    /// Coordinator failure even when it occurred before any state event could be published.
    pub error: Option<DisksGraphError>,
}

/// Process-local run evidence; saved JSON cannot reconstruct this owner or restart its native effects.
pub(crate) struct RunRecord {
    /// Sole mutable lifecycle state, now retained read-only by the facade.
    pub(crate) run: ExecutionRun,
    /// Trusted metadata copied only from the live FrozenPlan.
    source_map: Vec<OperationActions>,
    /// Preserves errors before dispatch as well as errors after checkpoint publication.
    error: Option<DisksGraphError>,
}
impl<H: RuntimeHost, D: BackendDispatcher> DisksGraph<H, D> {
    /// Executes one attempt synchronously using current host policies, not permission from saved JSON.
    /// Ok returns an inspectable run ID; only its snapshot's verified flag establishes actual success.
    /// Err retains the attempt and returns its ID in the diagnostic; there is no automatic retry API.
    pub fn apply_blocking(&mut self, document: PlanDocument) -> Result<RunId, DisksGraphError> {
        self.apply_document(document, vec![])
    }
    /// Executes a live frozen revision with its trusted action-to-operation ownership retained for clients.
    pub fn apply_frozen(&mut self, frozen: &FrozenPlan) -> Result<RunId, DisksGraphError> {
        self.apply_document(frozen.document().clone(), frozen.source_map().to_vec())
    }
    /// Applies a stored historical document with fresh readiness/admission and no fabricated source map.
    pub fn apply_plan(&mut self, plan: PlanId) -> Result<RunId, DisksGraphError> {
        self.apply_blocking(self.export_plan(plan)?)
    }
    /// Applies historical input with borrowed synchronous events; observer failure grants no permission.
    /// Notifications finish before this call returns, and retained snapshots/events remain available.
    pub fn apply_plan_with_events(
        &mut self,
        plan: PlanId,
        observer: &mut dyn FnMut(&RunEvent),
    ) -> Result<RunId, DisksGraphError> {
        let mut cursor = 0;
        self.apply_document_with_updates(self.export_plan(plan)?, vec![], &mut |run| {
            for event in run.events_since(cursor) {
                observer(event);
                cursor = event.sequence;
            }
        })
    }
    /// Publishes passive complete state and bounded events while the same coordinator is executing.
    /// Live snapshots always have finished=false; a final update includes coordinator errors and finished=true.
    pub fn apply_plan_with_updates(
        &mut self,
        plan: PlanId,
        observer: &mut dyn FnMut(RunSnapshot, EventsPage),
    ) -> Result<RunId, DisksGraphError> {
        let result =
            self.apply_document_with_updates(self.export_plan(plan)?, vec![], &mut |run| {
                let mut execution = run.snapshot();
                execution.finished = false;
                let events: Vec<_> = run.events_since(0).cloned().collect();
                let page = EventsPage {
                    gap: events.first().is_some_and(|event| event.sequence > 1),
                    next_sequence: execution.event_sequence,
                    events,
                };
                observer(
                    RunSnapshot {
                        execution,
                        source_map: vec![],
                        error: None,
                    },
                    page,
                );
            });
        if let Some(id) = result
            .as_ref()
            .ok()
            .copied()
            .or_else(|| result.as_ref().err().and_then(|error| error.run_id))
        {
            observer(self.run_snapshot(id)?, self.events(id, 0)?);
        }
        result
    }
    /// Returns the complete current evidence even after event eviction or a failure before the first event.
    pub fn run_snapshot(&self, id: RunId) -> Result<RunSnapshot, DisksGraphError> {
        let record = self.run(id)?;
        let mut execution = record.run.snapshot();
        // Every record here is published only after its blocking coordinator call has ended.
        execution.finished = true;
        Ok(RunSnapshot {
            execution,
            source_map: record.source_map.clone(),
            error: record.error.clone(),
        })
    }
    /// Replays retained updates using an exclusive cursor; after gap, fetch a snapshot and use its cursor.
    pub fn events(&self, id: RunId, since: u64) -> Result<EventsPage, DisksGraphError> {
        Ok(self.run(id)?.run.events_page(since)?)
    }
    /// One instance-owned lookup prevents another application's handle from exposing execution state.
    fn run(&self, id: RunId) -> Result<&RunRecord, DisksGraphError> {
        self.runs.get(&id).ok_or_else(|| {
            DisksGraphError::new(ErrorCode::UnknownRun, "run is not owned by this instance")
        })
    }
    /// Reserves retention before effects, then borrows the sole configured host/dispatcher under the coordinator.
    fn apply_document(
        &mut self,
        document: PlanDocument,
        source_map: Vec<OperationActions>,
    ) -> Result<RunId, DisksGraphError> {
        self.apply_document_with_updates(document, source_map, &mut |_| {})
    }
    /// Event delivery shares the exact reservation, coordinator and failure-retention path.
    fn apply_document_with_updates(
        &mut self,
        document: PlanDocument,
        source_map: Vec<OperationActions>,
        observer: &mut dyn FnMut(&ExecutionRun),
    ) -> Result<RunId, DisksGraphError> {
        if self.runs.len() >= MAX_APPLICATION_OBJECTS {
            return Err(DisksGraphError::new(
                ErrorCode::ResourceLimit,
                "run limit reached",
            ));
        }
        self.runs
            .try_reserve(1)
            .map_err(|error| DisksGraphError::new(ErrorCode::ResourceLimit, error))?;
        let mut run = ExecutionRun::new(&document)?;
        let id = RunId(run.run_id());
        let result = self
            .root
            .try_clone()
            .map_err(DisksGraphError::from)
            .and_then(|root| {
                let mut coordinator =
                    ExecutionCoordinator::new(root, &mut self.host, &mut self.dispatcher, &run);
                coordinator
                    .execute_run_with_updates(&mut run, observer)
                    .map_err(DisksGraphError::from)
            });
        let error = result.err().map(|mut error| {
            error.run_id = Some(id);
            error
        });
        self.runs.insert(
            id,
            RunRecord {
                run,
                source_map,
                error: error.clone(),
            },
        );
        match error {
            Some(error) => Err(error),
            None => Ok(id),
        }
    }
}
