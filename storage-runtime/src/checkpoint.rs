//! A bounded description of one possibly interrupted action; never executable recovery state.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use storage_core::model::{ExternalId, FilesystemKind, NodeId};
use storage_engine::{
    ActionId, ActionRunState, ExecutionRun, ExecutionRunError, PlanDocument, PlanDocumentError,
    editing::OperationId,
};
use storage_provider::{BackendId, FilesystemAction, LvmAction, ProviderAction};
use thiserror::Error;
use uuid::Uuid;

#[cfg(target_os = "linux")]
mod store;
#[cfg(target_os = "linux")]
pub use store::CheckpointStore;

/// Initial checkpoint schema, independent of plan and provider-action versions.
pub const CHECKPOINT_SCHEMA_VERSION: u16 = 1;
/// Maximum encoded checkpoint size, enforced before parsing or publication.
pub const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
/// Maximum number of affected identities retained in one attempt.
const MAX_TARGETS: usize = 64;
/// Maximum external identity count for one target.
const MAX_EXTERNAL_IDS: usize = 16;
/// Maximum UTF-8 byte length of any retained string, including action parameters.
const MAX_STRING_BYTES: usize = 1024;

/// Failures leave the runtime without permission to invoke a storage backend.
#[derive(Debug, Error)]
pub enum CheckpointError {
    /// The retained execution root or permanent lock no longer satisfies its safety contract.
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Lease(#[from] crate::lease::LeaseError),
    /// A file could not be read, published, synchronized, or removed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Encoded data is malformed; it must never be interpreted as absence.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Checkpoint versions are not silently migrated or ignored.
    #[error("unsupported checkpoint schema {0}")]
    SchemaVersion(u16),
    /// One of the fixed byte, string, or collection limits was exceeded.
    #[error("checkpoint exceeds its limits")]
    TooLarge,
    /// Required identity or binding data is absent or inconsistent.
    #[error("invalid checkpoint: {0}")]
    InvalidData(&'static str),
    /// Only parameters reviewed for the narrow LVM and ext4 recipes may be persisted.
    #[error("checkpoint action is not supported")]
    UnsupportedAction,
    /// Capture must occur after full admission and before the invocation boundary.
    #[error("checkpoint capture requires an Admitted action")]
    NotAdmitted,
    /// An unresolved attempt already occupies the only active checkpoint name.
    #[error("an active checkpoint already exists")]
    AlreadyActive,
    /// Clear requires an existing checkpoint with the exact requested run/action identity.
    #[error("checkpoint does not belong to this run and action")]
    AttemptMismatch,
    /// Files must be private, non-executable, without special bits, owned and singly linked.
    #[error("invalid checkpoint file")]
    InvalidFile,
    /// A failed mutation may have changed durable state; the host must stop this run.
    #[error("checkpoint store cannot mutate after an I/O failure")]
    Poisoned,
    /// Run identity or state could not be read.
    #[error(transparent)]
    Run(#[from] ExecutionRunError),
    /// The exact immutable plan could not be validated or canonicalized.
    #[error(transparent)]
    Document(#[from] PlanDocumentError),
}

/// Stable identity observed before dispatch; paths and device numbers are insufficient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetFingerprint {
    /// Identity in the immutable plan, not a newly allocated probe NodeId.
    pub planned_id: NodeId,
    /// External identities confirmed by the trusted runtime in its fresh probe.
    pub external_ids: Vec<ExternalId>,
}

/// A single action whose dispatch may have begun; this is neither history nor authorization.
/// No runtime phase, full graph, observation, or credential material is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveAttempt {
    /// Envelope version checked before accepting the record.
    pub schema_version: u16,
    /// Execution attempt that owns the checkpoint.
    pub run_id: Uuid,
    /// Immutable reviewed plan identity.
    pub plan_id: Uuid,
    /// Exact action within that plan.
    pub action_id: ActionId,
    /// Optional editing-operation identity, used only for diagnostics.
    pub operation_id: Option<OperationId>,
    /// Non-secret parameters of a supported action, not a command to replay.
    pub action: ProviderAction,
    /// Confirmed pre-dispatch identities, never proof of current state after a restart.
    pub targets: Vec<TargetFingerprint>,
    /// Lowercase SHA-256 of the exact canonical PlanDocument; never an approval token.
    pub plan_digest: String,
    /// Concrete backend selected by the trusted runtime.
    pub backend_id: BackendId,
}

/// Native identity required for the operation's primary target, independent of other fingerprints.
#[derive(Clone, Copy)]
enum PrimaryIdentity {
    /// Existing VG for LV creation.
    VolumeGroup,
    /// Verified LV for activation or formatting.
    LogicalVolume,
    /// Verified ext4 output for Host mounting.
    Filesystem,
}

impl ActiveAttempt {
    /// Captures an admitted action and exact plan; the host must first resolve target fingerprints.
    /// This validates data only and neither publishes a file nor grants write authorization.
    pub fn capture(
        run: &ExecutionRun,
        action_id: ActionId,
        operation_id: Option<OperationId>,
        targets: Vec<TargetFingerprint>,
        backend_id: BackendId,
    ) -> Result<Self, CheckpointError> {
        if run.state(action_id)? != ActionRunState::Admitted {
            return Err(CheckpointError::NotAdmitted);
        }
        let action = run.context().plan.actions()[action_id.get() as usize]
            .operation
            .clone();
        let attempt = Self {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            run_id: run.run_id(),
            plan_id: run.document().plan_id,
            action_id,
            operation_id,
            action,
            targets,
            plan_digest: plan_digest(run.document())?,
            backend_id,
        };
        attempt.to_bytes()?;
        Ok(attempt)
    }

    /// Decodes bounded data, rejecting unknown fields even inside reused provider-action types.
    /// Imported data is diagnostic only and cannot reconstruct an ExecutionRun.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CheckpointError> {
        if bytes.len() > MAX_CHECKPOINT_BYTES {
            return Err(CheckpointError::TooLarge);
        }
        let attempt: Self = serde_json::from_slice(bytes)?;
        attempt.validate()?;
        let input: serde_json::Value = serde_json::from_slice(bytes)?;
        if input != serde_json::to_value(&attempt)? {
            return Err(CheckpointError::InvalidData(
                "unrecognized or noncanonical fields",
            ));
        }
        Ok(attempt)
    }

    /// Validates the complete envelope before a store may publish any bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, CheckpointError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_CHECKPOINT_BYTES {
            return Err(CheckpointError::TooLarge);
        }
        Ok(bytes)
    }

    /// Checks schema, non-secret action scope, stable bindings, and fixed allocation limits.
    fn validate(&self) -> Result<(), CheckpointError> {
        if self.schema_version != CHECKPOINT_SCHEMA_VERSION {
            return Err(CheckpointError::SchemaVersion(self.schema_version));
        }
        if self.run_id.is_nil()
            || self.plan_id.is_nil()
            || self.operation_id.is_some_and(|id| id.as_uuid().is_nil())
        {
            return Err(CheckpointError::InvalidData("nil attempt identity"));
        }
        if self.plan_digest.len() != 64
            || !self
                .plan_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(CheckpointError::InvalidData("invalid SHA-256 plan digest"));
        }
        // Every added action needs an explicit non-secret persistence rule.
        let (primary, identity_kind) = match &self.action {
            ProviderAction::Lvm(LvmAction::CreateLv {
                vg,
                planned_node_id,
                size,
                ..
            }) if !planned_node_id.as_uuid().is_nil() && size.as_u64() != 0 => {
                (*vg, PrimaryIdentity::VolumeGroup)
            }
            ProviderAction::Lvm(LvmAction::SetActivation { target, .. }) => {
                (*target, PrimaryIdentity::LogicalVolume)
            }
            ProviderAction::Filesystem(FilesystemAction::Create {
                backing,
                planned_node_id,
                filesystem: FilesystemKind::Ext4,
                label,
            }) if !planned_node_id.as_uuid().is_nil()
                && planned_node_id != backing
                && label
                    .as_ref()
                    .is_none_or(|label| label.len() <= 16 && !label.contains('\0')) =>
            {
                (*backing, PrimaryIdentity::LogicalVolume)
            }
            storage_provider::ProviderAction::Mount(storage_provider::MountAction::Mount {
                source: storage_core::model::MountSource::Filesystem(fs),
                ..
            }) if storage_provider::is_ext4_host_mount(&self.action) => {
                (*fs, PrimaryIdentity::Filesystem)
            }
            _ => return Err(CheckpointError::UnsupportedAction),
        };
        if self.targets.is_empty()
            || !self
                .targets
                .iter()
                .any(|target| target.planned_id == primary)
        {
            return Err(CheckpointError::InvalidData(
                "missing primary target fingerprint",
            ));
        }
        if self.targets.len() > MAX_TARGETS {
            return Err(CheckpointError::TooLarge);
        }
        for (index, target) in self.targets.iter().enumerate() {
            if target.planned_id.as_uuid().is_nil()
                || self.targets[..index]
                    .iter()
                    .any(|other| other.planned_id == target.planned_id)
                || target.external_ids.is_empty()
            {
                return Err(CheckpointError::InvalidData(
                    "missing or duplicate target identity",
                ));
            }
            if target.external_ids.len() > MAX_EXTERNAL_IDS {
                return Err(CheckpointError::TooLarge);
            }
            if target.planned_id == primary
                && !target
                    .external_ids
                    .iter()
                    .any(|id| match (identity_kind, id) {
                        (PrimaryIdentity::VolumeGroup, ExternalId::LvmVgUuid(value)) => {
                            !value.trim().is_empty()
                        }
                        (PrimaryIdentity::LogicalVolume, ExternalId::LvmLvUuid(value)) => {
                            !value.trim().is_empty()
                        }
                        (
                            PrimaryIdentity::Filesystem,
                            ExternalId::Filesystem { fs_type, value },
                        ) => {
                            fs_type == "ext4"
                                && uuid::Uuid::parse_str(value)
                                    .is_ok_and(|uuid| !uuid.is_nil() && uuid.to_string() == *value)
                        }
                        _ => false,
                    })
            {
                return Err(CheckpointError::InvalidData(
                    "primary target requires its native UUID",
                ));
            }
        }
        check_string_limits(&serde_json::to_value(self)?)
    }
}

/// Bounds retained strings throughout nested action parameters and external identities.
fn check_string_limits(value: &serde_json::Value) -> Result<(), CheckpointError> {
    match value {
        serde_json::Value::String(text) if text.len() > MAX_STRING_BYTES => {
            return Err(CheckpointError::TooLarge);
        }
        serde_json::Value::Array(values) => {
            for value in values {
                check_string_limits(value)?;
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                check_string_limits(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// SHA-256 of a validated canonical plan, shared by approval requests and crash checkpoints.
/// A digest binds data only; it cannot establish authorization or probe freshness.
pub fn plan_digest(document: &PlanDocument) -> Result<String, PlanDocumentError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(document.canonical_json()?.as_bytes())
    ))
}
