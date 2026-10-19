//! Explicit ext4-only native adapter; host exclusion and destructive approval remain independent.

use crate::{
    capability::{BackendCapabilityCheck, OperationAvailability},
    dispatcher::{BackendDispatcher, DispatchError, ExecutionReceipt},
    endpoint::LvBlockEndpoint,
    targets::ResolvedAction,
};
use storage_core::model::FilesystemKind;
use storage_engine::{BackendOutcome, ExecutionRunError, LiveObservation};
use storage_provider::{
    BackendId, Ext4CreateRequest, FilesystemAction, FilesystemWriteError, ProviderAction,
    RegisteredBackend,
};

/// Retains one execute-enabled libblockdev-fs registration for availability and dispatch.
/// Default scope/consumer coverage remains Unknown; construction alone never admits a write.
#[derive(Clone, Copy)]
pub struct NativeExt4Adapter<'backend> {
    /// Explicit registration, with no fallback to another library or executable.
    backend: &'backend RegisteredBackend,
}
impl<'backend> NativeExt4Adapter<'backend> {
    /// Requires the complete ext4 MKFS/QUERY recipe before this adapter can be configured.
    pub fn new(backend: &'backend RegisteredBackend) -> Result<Self, FilesystemWriteError> {
        backend.check_ext4_create_support()?;
        Ok(Self { backend })
    }
}
impl BackendCapabilityCheck for NativeExt4Adapter<'_> {
    /// Checks future outputs without requiring their block paths to exist before LV creation.
    fn availability(
        &self,
        operation: &ProviderAction,
        _: &LiveObservation,
    ) -> OperationAvailability {
        match operation {
            ProviderAction::Filesystem(FilesystemAction::Create {
                filesystem: FilesystemKind::Ext4,
                label,
                ..
            }) if label.as_ref().is_none_or(|label| {
                !label.is_empty() && label.len() <= 16 && !label.contains('\0')
            }) => {}
            _ => {
                return OperationAvailability::Unavailable(
                    "native filesystem adapter supports only narrow ext4 creation".into(),
                );
            }
        }
        match self.backend.check_ext4_create_support() {
            Ok(()) => OperationAvailability::Available(self.backend.manifest().id.clone()),
            Err(error) => OperationAvailability::Unavailable(error.to_string()),
        }
    }
}
impl BackendDispatcher for NativeExt4Adapter<'_> {
    /// Matches the exact registration checked during operation availability.
    fn backend_id(&self) -> &BackendId {
        &self.backend.manifest().id
    }

    /// Invokes once through a retained, rechecked LV descriptor and returns independently read native UUID.
    /// The host must exclude external writers and DM changes throughout this call and result verification.
    fn execute(
        &mut self,
        action: &ResolvedAction,
        _: &mut dyn FnMut(String) -> Result<(), ExecutionRunError>,
    ) -> Result<ExecutionReceipt, DispatchError> {
        if action.backend() != self.backend_id() {
            return Err(DispatchError::Adapter(
                "native filesystem backend mismatch".into(),
            ));
        }
        let ProviderAction::Filesystem(FilesystemAction::Create {
            filesystem: FilesystemKind::Ext4,
            label,
            ..
        }) = action.operation()
        else {
            return Err(DispatchError::Adapter(
                "unsupported native filesystem operation".into(),
            ));
        };
        let target = action
            .lv_target()
            .ok_or_else(|| DispatchError::Adapter("ext4 requires a verified LV endpoint".into()))?;
        let request = Ext4CreateRequest::new(
            target.size.as_u64(),
            &uuid::Uuid::now_v7().to_string(),
            label.as_deref(),
        )
        .map_err(|error| DispatchError::Adapter(error.to_string()))?;
        let endpoint = LvBlockEndpoint::open(target, &action.target().vg_uuid)
            .map_err(|error| DispatchError::Adapter(error.to_string()))?;
        let uuid = self
            .backend
            .create_ext4(endpoint.file(), &request)
            .map_err(|error| DispatchError::Adapter(error.to_string()))?;
        endpoint
            .recheck()
            .map_err(|error| DispatchError::Adapter(error.to_string()))?;
        Ok(ExecutionReceipt {
            outcome: BackendOutcome::Succeeded,
            created_lv_uuid: None,
            created_filesystem_uuid: Some(uuid),
        })
    }
}
