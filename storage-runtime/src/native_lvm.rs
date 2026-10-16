//! Explicit native linear-create/activation adapter; scope and authorization are independent policies.

use crate::{
    capability::{BackendCapabilityCheck, OperationAvailability},
    dispatcher::{BackendDispatcher, DispatchError, ExecutionReceipt},
    targets::ResolvedAction,
};
use storage_core::model::LvmLvKind;
use storage_engine::{BackendOutcome, ExecutionRunError, LiveObservation};
use storage_provider::{
    BackendId, LinearLvRequest, LvActivationRequest, LvmAction, LvmWriteError, ProviderAction,
    RegisteredBackend,
};

/// One retained native registration used for both operation availability and synchronous dispatch.
/// Default scope coverage is Unknown; this adapter alone cannot authorize any write.
#[derive(Clone, Copy)]
pub struct NativeLvmAdapter<'backend> {
    /// Trusted manifest registration; no fallback or library selection occurs during dispatch.
    backend: &'backend RegisteredBackend,
}

impl<'backend> NativeLvmAdapter<'backend> {
    /// Requires an explicit execute manifest and the exact native recipe before host construction.
    pub fn new(backend: &'backend RegisteredBackend) -> Result<Self, LvmWriteError> {
        backend.check_linear_lv_support()?;
        Ok(Self { backend })
    }
}

impl BackendCapabilityCheck for NativeLvmAdapter<'_> {
    /// Rejects every unimplemented operation before checkpoint publication or native invocation.
    fn availability(
        &self,
        operation: &ProviderAction,
        _observed: &LiveObservation,
    ) -> OperationAvailability {
        let support = match operation {
            ProviderAction::Lvm(LvmAction::CreateLv {
                layout: LvmLvKind::Linear,
                ..
            }) => self.backend.check_linear_lv_support(),
            ProviderAction::Lvm(LvmAction::SetActivation { active: true, .. }) => {
                self.backend.check_lv_activation_support()
            }
            _ => {
                return OperationAvailability::Unavailable(
                    "native adapter supports only linear CreateLv and verified LV activation"
                        .into(),
                );
            }
        };
        match support {
            Ok(()) => OperationAvailability::Available(self.backend.manifest().id.clone()),
            Err(error) => OperationAvailability::Unavailable(error.to_string()),
        }
    }
}

impl BackendDispatcher for NativeLvmAdapter<'_> {
    /// Returns the same backend used for native technology and symbol checks during admission.
    fn backend_id(&self) -> &BackendId {
        &self.backend.manifest().id
    }

    /// Calls the admitted linear recipe once; native errors remain uncertain and are never retried.
    /// No progress is invented for this synchronous C API.
    fn execute(
        &mut self,
        action: &ResolvedAction,
        _progress: &mut dyn FnMut(String) -> Result<(), ExecutionRunError>,
    ) -> Result<ExecutionReceipt, DispatchError> {
        if action.backend() != self.backend_id() {
            return Err(DispatchError::Adapter("native backend mismatch".into()));
        }
        let target = action.target();
        match action.operation() {
            ProviderAction::Lvm(LvmAction::CreateLv {
                name,
                size,
                layout: LvmLvKind::Linear,
                ..
            }) => {
                let request =
                    LinearLvRequest::new(&target.vg_name, &target.vg_uuid, name, size.as_u64())
                        .map_err(|error| DispatchError::Adapter(error.to_string()))?;
                let uuid = self
                    .backend
                    .create_linear_lv(&request)
                    .map_err(|error| DispatchError::Adapter(error.to_string()))?;
                Ok(ExecutionReceipt {
                    outcome: BackendOutcome::Succeeded,
                    created_lv_uuid: Some(uuid),
                })
            }
            ProviderAction::Lvm(LvmAction::SetActivation { active: true, .. }) => {
                let lv = action.lv_target().ok_or_else(|| {
                    DispatchError::Adapter("activation requires a verified LV endpoint".into())
                })?;
                let request = LvActivationRequest::new(
                    &target.vg_name,
                    &target.vg_uuid,
                    &lv.lv_name,
                    &lv.lv_uuid,
                    lv.size.as_u64(),
                )
                .map_err(|error| DispatchError::Adapter(error.to_string()))?;
                self.backend
                    .activate_lv(&request)
                    .map_err(|error| DispatchError::Adapter(error.to_string()))?;
                Ok(ExecutionReceipt {
                    outcome: BackendOutcome::Succeeded,
                    created_lv_uuid: None,
                })
            }
            _ => Err(DispatchError::Adapter(
                "unsupported native LVM operation".into(),
            )),
        }
    }
}
