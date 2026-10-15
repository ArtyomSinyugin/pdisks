//! Separate display-only previews, editable drafts, and frozen execution descriptions.

use serde::{Deserialize, Serialize};
use storage_core::model::DesiredState;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    HistoricalBaseline, IntentCoverage, ProtectionViolation, SimulatedStep,
    SimulationEvidenceSummary, SimulationObligation,
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

/// Version of persisted editable requests, separate from display and execution schemas.
pub const DRAFT_SCHEMA_VERSION: u32 = 1;

/// Maximum UTF-8 size accepted by the draft JSON boundary, including its historical snapshot.
pub const MAX_DRAFT_JSON_BYTES: usize = 1024 * 1024;

/// Maximum number of operations accepted in a persisted draft.
pub const MAX_DRAFT_OPERATIONS: usize = 256;

/// Saved editable inputs only; actions and future state are always recalculated on opening.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftDocument {
    /// Version of this envelope and its typed request catalogue.
    pub schema_version: u32,
    /// Identity preserved while resuming the same local editing session.
    pub session_id: Uuid,
    /// Publication revision retained independently of discarded undo history.
    pub revision: DraftRevision,
    /// Observed state and capacity from the same historical context.
    pub baseline: HistoricalBaseline,
    /// Stable user requests and planned output identities, including invalid semantic requests.
    pub draft: super::PlanDraft,
}

/// Import/export failure at the untrusted draft boundary; no system write has occurred.
#[derive(Debug, Error)]
pub enum DraftDocumentError {
    /// Invalid JSON, unknown fields, typed shape, or serde_json's default nesting limit.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The request/document schema is unsupported.
    #[error("unsupported draft schema version {0}")]
    SchemaVersion(u64),
    /// A saved session must retain a non-nil identity.
    #[error("draft session identity must not be nil")]
    InvalidSessionIdentity,
    /// Input exceeds the byte bound before typed deserialization.
    #[error("draft JSON exceeds {MAX_DRAFT_JSON_BYTES} bytes")]
    TooLarge,
    /// The operation list exceeds the offline editor's bounded input size.
    #[error("draft contains more than {MAX_DRAFT_OPERATIONS} operations")]
    TooManyOperations,
    /// Invalid baseline, operation/output identities, references, or an internal build failure.
    #[error("invalid draft inputs: {0}")]
    Inputs(Box<DraftBuildError>),
}

impl DraftDocument {
    /// Parses bounded JSON with the default 128-level recursion limit, then validates identities.
    /// Version checks precede request decoding; the original JSON is decoded again to reject duplicates.
    pub fn from_json(json: &str) -> Result<Self, DraftDocumentError> {
        if json.len() > MAX_DRAFT_JSON_BYTES {
            return Err(DraftDocumentError::TooLarge);
        }
        let value: serde_json::Value = serde_json::from_str(json)?;
        if let Some(version) = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            && version != u64::from(DRAFT_SCHEMA_VERSION)
        {
            return Err(DraftDocumentError::SchemaVersion(version));
        }
        if value
            .pointer("/draft/operations")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|operations| operations.len() > MAX_DRAFT_OPERATIONS)
        {
            return Err(DraftDocumentError::TooManyOperations);
        }
        let document: Self = serde_json::from_str(json)?;
        document.validate()?;
        Ok(document)
    }

    /// Saves structurally valid inputs even when their semantic plan is invalid or unsupported.
    pub fn to_json(&self) -> Result<String, DraftDocumentError> {
        self.validate()?;
        Ok(serde_json::to_string(self)?)
    }

    /// Checks public/deserialized data again before it can become a live editing session.
    pub(super) fn validate(&self) -> Result<(), DraftDocumentError> {
        if self.schema_version != DRAFT_SCHEMA_VERSION {
            return Err(DraftDocumentError::SchemaVersion(u64::from(
                self.schema_version,
            )));
        }
        if self.session_id.is_nil() {
            return Err(DraftDocumentError::InvalidSessionIdentity);
        }
        if self.draft.operations.len() > MAX_DRAFT_OPERATIONS {
            return Err(DraftDocumentError::TooManyOperations);
        }
        if serde_json::to_vec(self)?.len() > MAX_DRAFT_JSON_BYTES {
            return Err(DraftDocumentError::TooLarge);
        }
        super::build::validate_draft(&self.baseline, &self.draft)
            .map_err(|error| DraftDocumentError::Inputs(Box::new(error)))?;
        // Validate observation structure and capacity independently of any invalid user request.
        super::build_preview(&self.baseline, &super::PlanDraft::default())
            .map_err(|error| DraftDocumentError::Inputs(Box::new(error)))?;
        Ok(())
    }
}

impl PlanSession {
    /// Captures editable inputs; undo/redo, generated actions, and forecasts are not persisted.
    pub fn draft_document(&self) -> DraftDocument {
        DraftDocument {
            schema_version: DRAFT_SCHEMA_VERSION,
            session_id: self.id(),
            revision: self.revision(),
            baseline: self.baseline().clone(),
            draft: self.draft().clone(),
        }
    }
}

#[cfg(test)]
mod draft_tests {
    use super::*;
    use crate::editing::{
        EditError, PlanEdit, PlanRequest,
        build::tests::{baseline, lv_request, workflow},
    };
    use storage_core::model::{Bytes, FilesystemKind, NodeId};

    /// Resuming recalculates forecasts, retains all IDs/revisions, and starts with empty history.
    #[test]
    fn draft_round_trip_rebuilds_valid_and_semantically_invalid_previews() {
        let mut document = PlanSession::new(baseline())
            .unwrap_or_else(|error| panic!("new: {error}"))
            .draft_document();
        document.draft = workflow();
        document.revision = serde_json::from_value(serde_json::json!(5))
            .unwrap_or_else(|error| panic!("revision: {error}"));
        for invalid in [false, true] {
            if invalid
                && let PlanRequest::CreateLv { size, .. } =
                    &mut document.draft.operations[0].request
            {
                *size = Bytes::new(101 * 4 * 1024 * 1024);
            }
            let json = document
                .to_json()
                .unwrap_or_else(|error| panic!("save: {error}"));
            assert!(!json.contains("informational_contract"));
            let decoded =
                DraftDocument::from_json(&json).unwrap_or_else(|error| panic!("parse: {error}"));
            assert_eq!(decoded, document);
            let mut session =
                PlanSession::from_document(decoded).unwrap_or_else(|error| panic!("open: {error}"));
            assert_eq!(session.id(), document.session_id);
            assert_eq!(session.revision(), document.revision);
            assert_eq!(session.draft(), &document.draft);
            assert_eq!(session.baseline(), &document.baseline);
            assert_eq!(
                session.preview().status(),
                if invalid {
                    PreviewStatus::Invalid
                } else {
                    PreviewStatus::Conditional
                }
            );
            assert_eq!(session.undo(session.revision()), Err(EditError::NoUndo));
            let saved = session.draft_document();
            session
                .edit(session.revision(), PlanEdit::Clear)
                .unwrap_or_else(|error| panic!("edit resumed draft: {error}"));
            assert_eq!(session.revision().get(), 6);
            assert_eq!(saved.draft, document.draft);
        }
    }

    /// Version, identity, output, and reference validation applies to both JSON and typed inputs.
    #[test]
    fn draft_import_rejects_corrupt_inputs_and_forged_forecasts() {
        let mut original = PlanSession::new(baseline())
            .unwrap_or_else(|error| panic!("new: {error}"))
            .draft_document();
        original.draft = workflow();
        for case in 0..7 {
            let mut document = original.clone();
            match case {
                0 => document.session_id = Uuid::nil(),
                1 => document.draft.operations[1].id = document.draft.operations[0].id,
                2 => document.draft.operations[0].created_node = None,
                3 => {
                    document.draft.operations[1].created_node =
                        document.draft.operations[0].created_node
                }
                4 => {
                    document.draft.operations[1].request = PlanRequest::CreateFilesystem {
                        backing: NodeId::new(),
                        filesystem: FilesystemKind::Ext4,
                        label: None,
                    }
                }
                5 => document
                    .baseline
                    .capacity_evidence
                    .push(document.baseline.capacity_evidence[0].clone()),
                _ => document.schema_version += 1,
            }
            let json = serde_json::to_string(&document)
                .unwrap_or_else(|error| panic!("encode untrusted: {error}"));
            assert!(DraftDocument::from_json(&json).is_err());
            assert!(PlanSession::from_document(document).is_err());
        }
        let mut value =
            serde_json::to_value(&original).unwrap_or_else(|error| panic!("value: {error}"));
        value["preview"] = serde_json::json!({"status": "complete"});
        assert!(DraftDocument::from_json(&value.to_string()).is_err());
        value["schema_version"] = serde_json::json!(999);
        value["draft"] = serde_json::json!({"unknown_future_request": true});
        assert!(matches!(
            DraftDocument::from_json(&value.to_string()),
            Err(DraftDocumentError::SchemaVersion(999))
        ));
    }

    /// Byte, operation, and nesting bounds reject hostile JSON before engine replay.
    #[test]
    fn draft_import_bounds_size_count_and_depth() {
        assert!(matches!(
            DraftDocument::from_json(&" ".repeat(MAX_DRAFT_JSON_BYTES + 1)),
            Err(DraftDocumentError::TooLarge)
        ));
        let nested = format!("{}0{}", "[".repeat(129), "]".repeat(129));
        assert!(matches!(
            DraftDocument::from_json(&nested),
            Err(DraftDocumentError::Json(_))
        ));
        let mut document = PlanSession::new(baseline())
            .unwrap_or_else(|error| panic!("new: {error}"))
            .draft_document();
        document.draft.operations = (0..=MAX_DRAFT_OPERATIONS)
            .map(|_| crate::editing::PlanOperation::new(lv_request()))
            .collect();
        let json =
            serde_json::to_string(&document).unwrap_or_else(|error| panic!("encode: {error}"));
        assert!(matches!(
            DraftDocument::from_json(&json),
            Err(DraftDocumentError::TooManyOperations)
        ));
        assert!(matches!(
            PlanSession::from_document(document),
            Err(DraftDocumentError::TooManyOperations)
        ));
    }
}
