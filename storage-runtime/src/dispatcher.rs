//! Typed synchronous dispatch boundary; concrete adapters require explicit host configuration.

use crate::targets::ResolvedAction;
use storage_engine::{BackendOutcome, ExecutionRunError};
use storage_provider::BackendId;
use thiserror::Error;

/// A dispatch error does not prove absence of partial effects.
#[derive(Debug, Error)]
pub enum DispatchError {
    /// A native adapter could not classify completion safely.
    #[error("backend invocation failed: {0}")]
    Adapter(String),
    /// Progress could not be accepted by the owning execution run.
    #[error(transparent)]
    Progress(#[from] ExecutionRunError),
}

/// Completed adapter result; an optional native LV UUID never replaces a fresh provider pass.
#[derive(Debug)]
pub struct ExecutionReceipt {
    /// Classified synchronous completion, including partial failure or uncertainty.
    pub outcome: BackendOutcome,
    /// UUID returned by a trusted CreateLv adapter; names or planned NodeIds are not evidence.
    pub created_lv_uuid: Option<String>,
    /// UUID returned by a trusted filesystem create adapter; independently checked after probe.
    pub created_filesystem_uuid: Option<String>,
}

/// Trusted typed backend called only after admission and durable checkpoint publication.
/// Returns completed outcomes, not asynchronous launch acknowledgments; errors become unknown.
pub trait BackendDispatcher {
    /// Concrete adapter identity, checked against the admitted endpoint and checkpoint.
    fn backend_id(&self) -> &BackendId;
    /// Executes the immutable resolved action while the coordinator retains its OS lease.
    fn execute(
        &mut self,
        action: &ResolvedAction,
        progress: &mut dyn FnMut(String) -> Result<(), ExecutionRunError>,
    ) -> Result<ExecutionReceipt, DispatchError>;
}

impl ExecutionReceipt {
    /// Rejects identities from a different operation; missing identities remain verification gaps.
    /// A success flag never turns an LV UUID into filesystem creation evidence or vice versa.
    pub fn validate_for(
        &self,
        operation: &storage_provider::ProviderAction,
    ) -> Result<(), DispatchError> {
        use storage_provider::{FilesystemAction, LvmAction, ProviderAction};
        let lv_create = matches!(operation, ProviderAction::Lvm(LvmAction::CreateLv { .. }));
        let fs_create = matches!(
            operation,
            ProviderAction::Filesystem(FilesystemAction::Create { .. })
        );
        if (self.created_lv_uuid.is_some() && !lv_create)
            || (self.created_filesystem_uuid.is_some() && !fs_create)
        {
            return Err(DispatchError::Adapter(
                "receipt identity does not match the dispatched operation".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage_core::model::{FilesystemKind, NodeId};
    use storage_provider::{FilesystemAction, LvmAction, ProviderAction};

    /// Operation-specific receipt fields cannot be substituted even when a backend claims success.
    #[test]
    fn receipt_identities_are_operation_specific() {
        let operation = ProviderAction::Filesystem(FilesystemAction::Create {
            backing: NodeId::new(),
            planned_node_id: NodeId::new(),
            filesystem: FilesystemKind::Ext4,
            label: None,
        });
        let mut receipt = ExecutionReceipt {
            outcome: BackendOutcome::Succeeded,
            created_lv_uuid: None,
            created_filesystem_uuid: Some(uuid::Uuid::now_v7().to_string()),
        };
        assert!(receipt.validate_for(&operation).is_ok());
        let activation = ProviderAction::Lvm(LvmAction::SetActivation {
            target: NodeId::new(),
            active: true,
        });
        assert!(receipt.validate_for(&activation).is_err());
        receipt.created_lv_uuid = Some("lv-uuid".into());
        assert!(receipt.validate_for(&operation).is_err());
        receipt.created_lv_uuid = None;
        receipt.created_filesystem_uuid = None;
        assert!(receipt.validate_for(&operation).is_ok());
        assert!(receipt.validate_for(&activation).is_ok());
    }
}
