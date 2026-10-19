//! Narrow native Host mount of the same run's verified ext4 output.

use crate::{
    capability::{BackendCapabilityCheck, OperationAvailability},
    dispatcher::{BackendDispatcher, DispatchError, ExecutionReceipt},
    endpoint::{EndpointError, LvBlockEndpoint},
    targets::ResolvedAction,
};
use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
use std::{fs::File, os::fd::AsRawFd, path::Path};
use storage_engine::{BackendOutcome, ExecutionRunError, LiveObservation};
use storage_provider::{
    BackendId, MountAction, MountWriteError, ProviderAction, RegisteredBackend,
};

/// Pins an empty existing directory without following symlinks anywhere in the target path.
/// The trusted host must exclude directory-tree and namespace mutations through verification.
pub fn open_mount_target(target: &Path) -> Result<File, EndpointError> {
    if !target.is_absolute()
        || target.parent().is_none()
        || target.components().any(|part| {
            !matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
    {
        return Err(EndpointError::MountTarget);
    }
    let fd = openat2(
        CWD,
        target,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(std::io::Error::from)?;
    let file = File::from(fd);
    if std::fs::read_dir(format!("/proc/self/fd/{}", file.as_raw_fd()))?
        .next()
        .is_some()
    {
        return Err(EndpointError::MountTarget);
    }
    Ok(file)
}

/// Retains an explicitly execute-enabled libmount registration without granting namespace exclusion.
#[derive(Clone, Copy)]
pub struct NativeMountAdapter<'a> {
    /// Sole route for the helper-free Host recipe.
    backend: &'a RegisteredBackend,
}
impl<'a> NativeMountAdapter<'a> {
    /// Checks every write/status symbol and fixed context policy before configuration succeeds.
    pub fn new(backend: &'a RegisteredBackend) -> Result<Self, MountWriteError> {
        backend.check_ext4_mount_support()?;
        Ok(Self { backend })
    }
}
impl BackendCapabilityCheck for NativeMountAdapter<'_> {
    /// Supports only typed Host filesystem mounts with the exact bounded option profile.
    fn availability(
        &self,
        operation: &ProviderAction,
        _: &LiveObservation,
    ) -> OperationAvailability {
        if !storage_provider::is_ext4_host_mount(operation) {
            return OperationAvailability::Unavailable(
                "native mount supports only the fixed Host ext4 recipe".into(),
            );
        }
        match self.backend.check_ext4_mount_support() {
            Ok(()) => OperationAvailability::Available(self.backend.manifest().id.clone()),
            Err(error) => OperationAvailability::Unavailable(error.to_string()),
        }
    }
}
impl BackendDispatcher for NativeMountAdapter<'_> {
    /// Uses exactly the registration checked during admission.
    fn backend_id(&self) -> &BackendId {
        &self.backend.manifest().id
    }
    /// Mounts once using both pinned endpoints, retaining runtime's checkpoint on every failure.
    fn execute(
        &mut self,
        action: &ResolvedAction,
        _: &mut dyn FnMut(String) -> Result<(), ExecutionRunError>,
    ) -> Result<ExecutionReceipt, DispatchError> {
        if action.backend() != self.backend_id()
            || !storage_provider::is_ext4_host_mount(action.operation())
            || action.filesystem_target().is_none()
        {
            return Err(DispatchError::Adapter("unresolved Host ext4 mount".into()));
        }
        let ProviderAction::Mount(MountAction::Mount { target, .. }) = action.operation() else {
            return Err(DispatchError::Adapter("unsupported mount".into()));
        };
        let lv = action
            .lv_target()
            .ok_or_else(|| DispatchError::Adapter("missing mount backing LV".into()))?;
        let source = LvBlockEndpoint::open(lv, &action.target().vg_uuid)
            .map_err(|error| DispatchError::Adapter(error.to_string()))?;
        let target =
            open_mount_target(target).map_err(|error| DispatchError::Adapter(error.to_string()))?;
        self.backend
            .mount_ext4(source.file(), &target)
            .map_err(|error| DispatchError::Adapter(error.to_string()))?;
        source
            .recheck()
            .map_err(|error| DispatchError::Adapter(error.to_string()))?;
        Ok(ExecutionReceipt {
            outcome: BackendOutcome::Succeeded,
            created_lv_uuid: None,
            created_filesystem_uuid: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// No component may be a symlink, and a populated directory or filesystem root is never accepted.
    #[test]
    fn target_guard_pins_only_empty_real_directories() -> Result<(), Box<dyn std::error::Error>> {
        let root =
            std::env::temp_dir().join(format!("disksgraph-mount-target-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&root)?;
        let empty = root.join("empty");
        std::fs::create_dir(&empty)?;
        assert!(open_mount_target(&empty).is_ok());
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&empty, &alias)?;
        assert!(open_mount_target(&alias).is_err());
        let child = empty.join("child");
        std::fs::create_dir(&child)?;
        assert!(open_mount_target(&alias.join("child")).is_err());
        assert!(open_mount_target(&empty).is_err());
        assert!(open_mount_target(Path::new("/")).is_err());
        assert!(open_mount_target(Path::new("relative")).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
