//! Separate display-only previews, editable drafts, and frozen execution descriptions.

use serde::Serialize;
use storage_core::model::DesiredState;
use uuid::Uuid;

use crate::{
    IntentCoverage, ProtectionViolation, SimulatedStep, SimulationEvidenceSummary,
    SimulationObligation,
};

use super::{
    DraftBuildError, DraftRevision, OperationActions, OperationDiagnostic, PlanOperation,
    PlanSession, PreviewState, PreviewStatus, RequestCoverage,
};

/// Output preview schema, independent of editable drafts and frozen plan documents.
pub const PREVIEW_SCHEMA_VERSION: u32 = 1;

/// Borrowed view of one session revision; serialized output is never imported as a trusted plan.
#[derive(Debug, Serialize)]
pub struct PreviewDocument<'a> {
    /// Version of this display-only shape.
    pub schema_version: u32,
    /// Session whose requests produced this result.
    pub session_id: Uuid,
    /// Revision shared by operations and forecast.
    pub revision: DraftRevision,
    /// Planning status, without execution authorization.
    pub status: PreviewStatus,
    /// Current prediction, absent when this revision cannot be calculated.
    pub desired: Option<&'a DesiredState>,
    /// Qualifications of the same prediction, without fabricated live facts.
    pub evidence: Option<&'a SimulationEvidenceSummary>,
    /// Editable requests, retained even when the preview is invalid.
    pub operations: &'a [PlanOperation],
    /// Per-build links to requests, also available after composition failure.
    pub source_map: &'a [OperationActions],
    /// Action-local prerequisite results from the current simulation.
    pub steps: &'a [SimulatedStep],
    /// Missing evidence, including destructive approval that simulation cannot grant.
    pub obligations: &'a [SimulationObligation],
    /// Planned conflicts with the explicit preservation policy.
    pub protection_violations: &'a [ProtectionViolation],
    /// Coverage of explicit maintenance intents by the shared engine.
    pub intent_coverage: &'a [IntentCoverage],
    /// Independent exact coverage of user recipes.
    pub request_coverage: &'a [RequestCoverage],
    /// Warnings and attributed failures, including checks for rejected transitions.
    pub diagnostics: Vec<OperationDiagnostic>,
}

impl PreviewDocument<'_> {
    /// Encodes a display document without creating a new graph, plan, or identity.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

impl PlanSession {
    /// Exports only this revision's forecast; no predicted state is returned after a failed build.
    pub fn preview_document(&self) -> PreviewDocument<'_> {
        let preview = self.preview().calculated();
        let source_map = match self.preview() {
            PreviewState::Calculated(preview) => preview.source_map.as_slice(),
            PreviewState::Invalid(
                DraftBuildError::Composition { source_map, .. }
                | DraftBuildError::Simulation { source_map, .. },
            ) => source_map,
            PreviewState::Invalid(DraftBuildError::Draft { .. }) => &[],
        };
        PreviewDocument {
            schema_version: PREVIEW_SCHEMA_VERSION,
            session_id: self.id(),
            revision: self.revision(),
            status: self.preview().status(),
            desired: preview.map(|preview| &preview.report.simulation.desired),
            evidence: preview.map(|preview| &preview.report.simulation.evidence),
            operations: &self.draft().operations,
            source_map,
            steps: preview.map_or(&[], |preview| preview.report.simulation.steps.as_slice()),
            obligations: preview.map_or(&[], |preview| {
                preview.report.unresolved_obligations.as_slice()
            }),
            protection_violations: preview.map_or(&[], |preview| {
                preview.report.protection_violations.as_slice()
            }),
            intent_coverage: preview
                .map_or(&[], |preview| preview.report.intent_coverage.as_slice()),
            request_coverage: preview.map_or(&[], |preview| preview.request_coverage.as_slice()),
            diagnostics: self.preview().diagnostics(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editing::{
        PlanEdit, PlanRequest,
        build::tests::{baseline, lv_request},
    };
    use storage_core::model::Bytes;

    /// Output retains exact request ownership and unknown checks without a trusted import path.
    #[test]
    fn preview_json_tracks_current_revision_and_drops_failed_prediction() {
        let mut session =
            PlanSession::new(baseline()).unwrap_or_else(|error| panic!("new: {error}"));
        let empty = serde_json::to_value(session.preview_document())
            .unwrap_or_else(|error| panic!("empty: {error}"));
        assert_eq!(empty["status"], "complete");
        assert_eq!(empty["revision"], 0);
        assert!(!empty["desired"].is_null());
        let receipt = session
            .edit(session.revision(), PlanEdit::Add(lv_request()))
            .unwrap_or_else(|error| panic!("add: {error}"));
        let operation = receipt
            .added_operation
            .unwrap_or_else(|| panic!("operation ID"));
        let output = session
            .preview_document()
            .to_json()
            .unwrap_or_else(|error| panic!("preview: {error}"));
        let output: serde_json::Value =
            serde_json::from_str(&output).unwrap_or_else(|error| panic!("JSON: {error}"));
        assert_eq!(
            output["source_map"][0]["operation"],
            output["operations"][0]["id"]
        );
        assert!(
            !output["steps"]
                .as_array()
                .is_none_or(|steps| steps.is_empty())
        );
        let mut request = lv_request();
        if let PlanRequest::CreateLv { size, .. } = &mut request {
            *size = Bytes::new(101 * 4 * 1024 * 1024);
        }
        session
            .edit(session.revision(), PlanEdit::Replace { operation, request })
            .unwrap_or_else(|error| panic!("replace: {error}"));
        let invalid = serde_json::to_value(session.preview_document())
            .unwrap_or_else(|error| panic!("invalid: {error}"));
        assert_eq!(invalid["status"], "invalid");
        assert_eq!(invalid["revision"], 2);
        assert!(invalid["desired"].is_null());
        assert!(invalid["evidence"].is_null());
        assert_eq!(
            invalid["diagnostics"][0]["operation"],
            invalid["operations"][0]["id"]
        );
        assert!(
            invalid["diagnostics"][0]["checks"]
                .as_array()
                .is_some_and(|checks| !checks.is_empty())
        );
    }

    /// Unknown approval and blocked preservation are visible as separate output conditions.
    #[test]
    fn preview_output_keeps_obligations_and_protection_violations() {
        let mut session =
            PlanSession::new(baseline()).unwrap_or_else(|error| panic!("new: {error}"));
        let receipt = session
            .edit(session.revision(), PlanEdit::Add(lv_request()))
            .unwrap_or_else(|error| panic!("LV: {error}"));
        let backing = receipt.created_node.unwrap_or_else(|| panic!("LV node"));
        session
            .edit(
                session.revision(),
                PlanEdit::Add(PlanRequest::CreateFilesystem {
                    backing,
                    filesystem: storage_core::model::FilesystemKind::Ext4,
                    label: None,
                }),
            )
            .unwrap_or_else(|error| panic!("filesystem: {error}"));
        let conditional = serde_json::to_value(session.preview_document())
            .unwrap_or_else(|error| panic!("conditional: {error}"));
        assert_eq!(conditional["status"], "conditional");
        assert!(
            conditional["obligations"]
                .as_array()
                .is_some_and(|obligations| !obligations.is_empty())
        );
        session
            .edit(
                session.revision(),
                PlanEdit::SetProtection(crate::ProtectedScope {
                    nodes: vec![backing],
                    ..crate::ProtectedScope::default()
                }),
            )
            .unwrap_or_else(|error| panic!("protection: {error}"));
        let blocked = serde_json::to_value(session.preview_document())
            .unwrap_or_else(|error| panic!("blocked: {error}"));
        assert_eq!(blocked["status"], "blocked");
        assert!(!blocked["desired"].is_null());
        assert!(
            blocked["protection_violations"]
                .as_array()
                .is_some_and(|violations| !violations.is_empty())
        );
    }
}
