//! The first mount recipe: ext4, current host namespace, one explicit option profile.

use crate::{MountAction, ProviderAction};
use storage_core::model::{MountContext, MountSource};
use thiserror::Error;

/// Exact supported mount options; no fstab, helper, remount, force or read-only fallback.
pub const EXT4_HOST_MOUNT_OPTIONS: &[&str] = &["rw", "nosuid", "nodev", "noexec", "relatime"];

/// Native mount failures retain runtime's checkpoint even when the syscall outcome is uncertain.
#[derive(Debug, Error)]
pub enum MountWriteError {
    /// Execution requires an explicit trusted registration opt-in.
    #[error("mount backend does not declare execution capability")]
    ExecutionNotDeclared,
    /// Source or target descriptor no longer has the admitted file type.
    #[error("mount requires a block source and directory target")]
    Endpoint,
    /// Kernel descriptor metadata could not be read.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Native ABI setup or mount failed; success is still independently verified by runtime.
    #[error(transparent)]
    Native(#[from] crate::NativeProbeError),
}

/// Checks only representable intent; verified source ownership and namespace exclusion belong to runtime.
pub fn is_ext4_host_mount(action: &ProviderAction) -> bool {
    let ProviderAction::Mount(MountAction::Mount {
        source: MountSource::Filesystem(_),
        target,
        options,
        context: MountContext::Host,
    }) = action
    else {
        return false;
    };
    let mut actual: Vec<_> = options.iter().map(String::as_str).collect();
    let mut expected = EXT4_HOST_MOUNT_OPTIONS.to_vec();
    actual.sort_unstable();
    expected.sort_unstable();
    actual == expected
        && target.is_absolute()
        && target.parent().is_some()
        && target.components().all(|part| {
            matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
        && !target.as_os_str().as_encoded_bytes().contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Imported actions cannot introduce helpers, flags, namespaces or unbounded target addressing.
    #[test]
    fn mount_profile_is_exact_and_bounded() {
        let action = ProviderAction::Mount(MountAction::Mount {
            source: MountSource::Filesystem(storage_core::model::NodeId::new()),
            target: "/mnt/c4".into(),
            options: EXT4_HOST_MOUNT_OPTIONS
                .iter()
                .map(|option| (*option).into())
                .collect(),
            context: MountContext::Host,
        });
        assert!(is_ext4_host_mount(&action));
        for option in ["ro", "bind", "remount", "defaults", "exec", "rw"] {
            let mut changed = action.clone();
            if let ProviderAction::Mount(MountAction::Mount { options, .. }) = &mut changed {
                options.push(option.into());
            }
            assert!(!is_ext4_host_mount(&changed));
        }
        for target in ["/", "relative", "/mnt/../etc", "/mnt/bad\0target"] {
            let mut changed = action.clone();
            if let ProviderAction::Mount(MountAction::Mount { target: value, .. }) = &mut changed {
                *value = target.into();
            }
            assert!(!is_ext4_host_mount(&changed));
        }
    }
}
