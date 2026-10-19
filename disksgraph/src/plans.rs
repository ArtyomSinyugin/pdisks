//! Immutable plan storage, with source maps available only from live frozen sessions.

use crate::*;

/// Maximum UTF-8 plan JSON input accepted by the facade before parser allocation.
pub const MAX_PLAN_JSON_BYTES: usize = 16 * 1024 * 1024;

impl<H: RuntimeHost, D: BackendDispatcher> DisksGraph<H, D> {
    /// Freezes exactly the reviewed revision; Conditional remains conditional and grants no live approval.
    pub fn freeze(
        &self,
        session: SessionId,
        revision: DraftRevision,
    ) -> Result<FrozenPlan, DisksGraphError> {
        Ok(self.session(session)?.freeze(revision)?)
    }
    /// Rebuilds trusted contracts before retaining a historical plan; never creates a FrozenPlan or source map.
    pub fn import_plan(&mut self, document: PlanDocument) -> Result<PlanId, DisksGraphError> {
        if self.plans.len() >= MAX_APPLICATION_OBJECTS {
            return Err(DisksGraphError::new(
                ErrorCode::ResourceLimit,
                "plan limit reached",
            ));
        }
        document.analyse_historical()?;
        let id = PlanId(uuid::Uuid::now_v7());
        self.plans.insert(id, document);
        Ok(id)
    }
    /// Bounds saved input before parsing, then performs the same typed reanalysis as direct import.
    pub fn import_plan_json(&mut self, json: &str) -> Result<PlanId, DisksGraphError> {
        if json.len() > MAX_PLAN_JSON_BYTES {
            return Err(DisksGraphError::new(
                ErrorCode::ResourceLimit,
                "plan JSON is too large",
            ));
        }
        self.import_plan(PlanDocument::from_json(json)?)
    }
    /// Returns a copy; edits to the returned document cannot change the stored reviewed plan.
    pub fn export_plan(&self, plan: PlanId) -> Result<PlanDocument, DisksGraphError> {
        self.plans.get(&plan).cloned().ok_or_else(|| {
            DisksGraphError::new(ErrorCode::UnknownPlan, "plan is not owned by this instance")
        })
    }
}
