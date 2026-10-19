//! Explicit native routing for the supported LVM/ext4 prefix, without implicit scope or approval policies.

use crate::{
    capability::{BackendCapabilityCheck, OperationAvailability},
    dispatcher::{BackendDispatcher, DispatchError, ExecutionReceipt},
    native_ext4::NativeExt4Adapter,
    native_lvm::NativeLvmAdapter,
    targets::ResolvedAction,
};
use storage_engine::{ExecutionRunError, LiveObservation};
use storage_provider::{
    BackendId, FilesystemWriteError, LvmWriteError, ProviderAction, RegisteredBackend,
};
use thiserror::Error;

/// Configuration fails before execution if either explicitly selected native recipe is unavailable.
#[derive(Debug, Error)]
pub enum NativeStorageError {
    /// LVM registration cannot provide the linear-create recipe.
    #[error(transparent)]
    Lvm(#[from] LvmWriteError),
    /// Filesystem registration cannot provide the ext4 create/readback recipe.
    #[error(transparent)]
    Filesystem(#[from] FilesystemWriteError),
}

/// Two retained native registrations, fixed before planning readiness and never chosen by saved JSON.
/// All scope, consumer, destructive approval and final coverage policies remain fail-closed defaults.
#[derive(Clone, Copy)]
pub struct NativeStorageAdapter<'backend> {
    /// Sole route for implemented LVM operations.
    lvm: NativeLvmAdapter<'backend>,
    /// Sole route for implemented ext4 operations.
    filesystems: NativeExt4Adapter<'backend>,
}
impl<'backend> NativeStorageAdapter<'backend> {
    /// Requires execute opt-in and complete native recipes from both host-selected registrations.
    pub fn new(
        lvm: &'backend RegisteredBackend,
        filesystems: &'backend RegisteredBackend,
    ) -> Result<Self, NativeStorageError> {
        Ok(Self {
            lvm: NativeLvmAdapter::new(lvm)?,
            filesystems: NativeExt4Adapter::new(filesystems)?,
        })
    }
}
impl BackendCapabilityCheck for NativeStorageAdapter<'_> {
    /// Delegates support only to the fixed technology owner; unsupported operations never fall back.
    fn availability(
        &self,
        operation: &ProviderAction,
        observed: &LiveObservation,
    ) -> OperationAvailability {
        match operation {
            ProviderAction::Filesystem(_) => self.filesystems.availability(operation, observed),
            _ => self.lvm.availability(operation, observed),
        }
    }
}
impl BackendDispatcher for NativeStorageAdapter<'_> {
    /// Retains the LVM identity for callers using the legacy single-backend accessor.
    fn backend_id(&self) -> &BackendId {
        self.lvm.backend_id()
    }
    /// Returns the same concrete route used by capability checking and actual invocation.
    fn backend_for(&self, operation: &ProviderAction) -> &BackendId {
        match operation {
            ProviderAction::Filesystem(_) => self.filesystems.backend_id(),
            _ => self.lvm.backend_id(),
        }
    }
    /// Executes the admitted typed operation on its preselected adapter exactly once.
    fn execute(
        &mut self,
        action: &ResolvedAction,
        progress: &mut dyn FnMut(String) -> Result<(), ExecutionRunError>,
    ) -> Result<ExecutionReceipt, DispatchError> {
        if action.backend() != self.backend_for(action.operation()) {
            return Err(DispatchError::Adapter(
                "admitted backend differs from the configured native route".into(),
            ));
        }
        match action.operation() {
            ProviderAction::Filesystem(_) => self.filesystems.execute(action, progress),
            _ => self.lvm.execute(action, progress),
        }
    }
}
