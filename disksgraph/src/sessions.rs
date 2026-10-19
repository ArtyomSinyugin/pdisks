//! Revision-checked editing delegates entirely to the existing plan session.

use crate::*;
use storage_engine::editing::PlanSession;

impl<H: RuntimeHost, D: BackendDispatcher> DisksGraph<H, D> {
    /// Starts editing from one fresh graph/capacity publication; IDs come from the exported draft baseline.
    pub fn create_session(&mut self) -> Result<SessionId, DisksGraphError> {
        self.check_session_limit()?;
        let session = PlanSession::new(self.host.probe()?.into_baseline())?;
        Ok(self.insert_session(session))
    }
    /// Imports saved editable inputs, recomputing forecasts and discarding saved undo/redo history.
    /// A new facade handle is allocated even if another document has the same session UUID.
    pub fn import_draft(&mut self, document: DraftDocument) -> Result<SessionId, DisksGraphError> {
        self.check_session_limit()?;
        Ok(self.insert_session(PlanSession::from_document(document)?))
    }
    /// Returns the optimistic revision required for the next edit, undo, redo or freeze.
    pub fn revision(&self, session: SessionId) -> Result<DraftRevision, DisksGraphError> {
        Ok(self.session(session)?.revision())
    }
    /// Applies an atomic edit and preserves invalid/conditional previews for further correction.
    pub fn edit(
        &mut self,
        session: SessionId,
        revision: DraftRevision,
        edit: PlanEdit,
    ) -> Result<EditReceipt, DisksGraphError> {
        Ok(self.session_mut(session)?.edit(revision, edit)?)
    }
    /// Returns this revision's forecast, including errors, rather than an older successful forecast.
    pub fn preview(&self, session: SessionId) -> Result<&PreviewState, DisksGraphError> {
        Ok(self.session(session)?.preview())
    }
    /// Returns the serializable forecast for this revision, including Invalid and Conditional qualifications.
    pub fn preview_document(
        &self,
        session: SessionId,
    ) -> Result<PreviewDocument<'_>, DisksGraphError> {
        Ok(self.session(session)?.preview_document())
    }
    /// Exports bounded editing inputs; this does not save execution permission or trusted live evidence.
    pub fn export_draft(&self, session: SessionId) -> Result<DraftDocument, DisksGraphError> {
        Ok(self.session(session)?.draft_document())
    }
    /// Restores only prior user requests, never a completed disk operation.
    pub fn undo(
        &mut self,
        session: SessionId,
        revision: DraftRevision,
    ) -> Result<DraftRevision, DisksGraphError> {
        Ok(self.session_mut(session)?.undo(revision)?)
    }
    /// Reapplies only an undone editable draft.
    pub fn redo(
        &mut self,
        session: SessionId,
        revision: DraftRevision,
    ) -> Result<DraftRevision, DisksGraphError> {
        Ok(self.session_mut(session)?.redo(revision)?)
    }
    /// Lists the dependent requests that must be explicitly included in a removal.
    pub fn removal_impact(
        &self,
        session: SessionId,
        operation: OperationId,
    ) -> Result<Vec<OperationId>, DisksGraphError> {
        Ok(self.session(session)?.removal_impact(operation)?)
    }
    /// Prepares a fresh, separately reviewable rebase without changing the active draft.
    pub fn prepare_rebase(
        &mut self,
        session: SessionId,
        revision: DraftRevision,
    ) -> Result<RebasePreview, DisksGraphError> {
        let actual = self.revision(session)?;
        if actual != revision {
            return Err(storage_engine::editing::EditError::RevisionConflict {
                expected: revision,
                actual,
            }
            .into());
        }
        let baseline = self.host.probe()?.into_baseline();
        Ok(self.session(session)?.prepare_rebase(revision, baseline)?)
    }
    /// Accepts only a still-current reviewed candidate using the session's existing source checks.
    pub fn accept_rebase(
        &mut self,
        session: SessionId,
        revision: DraftRevision,
        candidate: RebasePreview,
    ) -> Result<DraftRevision, DisksGraphError> {
        Ok(self
            .session_mut(session)?
            .accept_rebase(revision, candidate)?)
    }
    /// Owns lookup and prevents a handle from another instance from reaching an editing object.
    pub(crate) fn session(&self, id: SessionId) -> Result<&PlanSession, DisksGraphError> {
        self.sessions.get(&id).ok_or_else(|| {
            DisksGraphError::new(
                ErrorCode::UnknownSession,
                "session is not owned by this instance",
            )
        })
    }
    /// Mutations have the same instance boundary as read-only views.
    fn session_mut(&mut self, id: SessionId) -> Result<&mut PlanSession, DisksGraphError> {
        self.sessions.get_mut(&id).ok_or_else(|| {
            DisksGraphError::new(
                ErrorCode::UnknownSession,
                "session is not owned by this instance",
            )
        })
    }
    /// Rejects allocation before probing so failed requests cannot consume unbounded provider work.
    fn check_session_limit(&self) -> Result<(), DisksGraphError> {
        if self.sessions.len() >= MAX_APPLICATION_OBJECTS {
            Err(DisksGraphError::new(
                ErrorCode::ResourceLimit,
                "session limit reached",
            ))
        } else {
            Ok(())
        }
    }
    /// Imported source identities are preserved inside the editor, independently of process-local handles.
    fn insert_session(&mut self, session: PlanSession) -> SessionId {
        let id = SessionId(uuid::Uuid::now_v7());
        self.sessions.insert(id, session);
        id
    }
}
