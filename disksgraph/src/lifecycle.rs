//! Explicit memory release and the unchanged runtime incident acknowledgment boundary.

use crate::*;
use storage_runtime::{
    authorization::AuthorizationContext, capability::BackendCapabilityCheck, host::LinuxLiveHost,
};

impl<H: RuntimeHost, D: BackendDispatcher> DisksGraph<H, D> {
    /// Releases only this instance's editor/history; frozen documents remain independent immutable values.
    pub fn close_session(&mut self, id: SessionId) -> Result<(), DisksGraphError> {
        self.sessions.remove(&id).map(|_| ()).ok_or_else(|| {
            DisksGraphError::new(
                ErrorCode::UnknownSession,
                "session is not owned by this instance",
            )
        })
    }
    /// Releases only the retained historical document, without changing any live run or disk state.
    pub fn close_plan(&mut self, id: PlanId) -> Result<(), DisksGraphError> {
        self.plans.remove(&id).map(|_| ()).ok_or_else(|| {
            DisksGraphError::new(ErrorCode::UnknownPlan, "plan is not owned by this instance")
        })
    }
    /// Releases a completed blocking attempt's memory; never acknowledges or removes its checkpoint.
    pub fn close_run(&mut self, id: RunId) -> Result<(), DisksGraphError> {
        self.runs.remove(&id).map(|_| ()).ok_or_else(|| {
            DisksGraphError::new(ErrorCode::UnknownRun, "run is not owned by this instance")
        })
    }
}
impl<C: BackendCapabilityCheck, A: AuthorizationContext, D: BackendDispatcher>
    DisksGraph<LinuxLiveHost<'_, C, A>, D>
{
    /// Inspects under the same host lock and configured providers without reconstructing a run.
    pub fn incident(&mut self) -> Result<Option<IncidentInspection>, DisksGraphError> {
        Ok(self.host.inspect_incident(&self.root)?)
    }
    /// Produces the runtime's immutable review and explicit confirmation challenge.
    pub fn review_incident(&mut self) -> Result<Option<IncidentReview>, DisksGraphError> {
        Ok(self.host.review_incident(&self.root)?)
    }
    /// Clears only after exact confirmation, fresh unchanged state and an independent trusted safety policy.
    /// It cannot mark the old run Verified, reconstruct it, retry it, or call a storage backend.
    pub fn acknowledge_incident(
        &mut self,
        review: IncidentReview,
        confirmation: uuid::Uuid,
        policy: &mut impl IncidentAcknowledgmentPolicy,
    ) -> Result<IncidentInspection, DisksGraphError> {
        Ok(self
            .host
            .acknowledge_incident(&self.root, review, confirmation, policy)?)
    }
}
