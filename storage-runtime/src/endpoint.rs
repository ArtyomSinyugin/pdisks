//! Retained Linux LV block descriptors; path stability never proves exclusion of external writers.

use crate::targets::ResolvedLvTarget;
use rustix::fs::{
    CWD, FileType, Mode, OFlags, fstat, ioctl_blkpbszget, ioctl_blksszget, major, minor, openat,
};
use std::{
    fs::File,
    io::{Seek, SeekFrom},
    os::fd::AsRawFd,
    path::{Component, Path},
};
use storage_core::model::BlockFacts;
use thiserror::Error;

/// Failure to establish the exact admitted LV endpoint prevents a native filesystem invocation.
#[derive(Debug, Error)]
pub enum EndpointError {
    /// Path, device identity, geometry, size or device-mapper UUID differs from admission.
    #[error("block descriptor does not match the admitted LV endpoint")]
    Changed,
    /// Mount target must be an empty existing directory pinned without symlink traversal.
    #[error("mount target is not an empty pinned directory")]
    MountTarget,
    /// A descriptor, ioctl or sysfs observation could not be obtained.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A retained block descriptor bound to one admitted LV, without a serialization or cloning API.
/// The host must exclude mounts, external writes and DM table changes throughout its lifetime.
/// This descriptor pins addressing; it is not an O_EXCL claim or a global writer lock.
#[derive(Debug)]
pub struct LvBlockEndpoint {
    /// Actual block device, opened only after an O_PATH descriptor established its file type.
    file: File,
    /// Major/minor and geometry from the admission publication.
    block: BlockFacts,
    /// Exact admitted LV bytes.
    size: u64,
    /// Kernel DM identity derived from the independently verified VG and LV UUIDs.
    dm_uuid: String,
}

impl LvBlockEndpoint {
    /// Opens the sole observed path without following its final symlink, then pins the actual inode.
    /// There is no fallback to another path or device when validation fails.
    pub fn open(target: &ResolvedLvTarget, vg_uuid: &str) -> Result<Self, EndpointError> {
        let dm_uuid = expected_dm_uuid(vg_uuid, &target.lv_uuid)?;
        let block = target.block.clone().ok_or(EndpointError::Changed)?;
        let [path] = block.paths.as_slice() else {
            return Err(EndpointError::Changed);
        };
        if !normalized_device_path(path)
            || block.read_only != Some(false)
            || block.geometry.is_none()
            || block.devno.major == 0
            || target.size.as_u64() == 0
        {
            return Err(EndpointError::Changed);
        }
        let path_fd = openat(
            CWD,
            path,
            OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let info = fstat(&path_fd).map_err(std::io::Error::from)?;
        if FileType::from_raw_mode(info.st_mode) != FileType::BlockDevice
            || major(info.st_rdev) != block.devno.major
            || minor(info.st_rdev) != block.devno.minor
        {
            return Err(EndpointError::Changed);
        }
        let pinned = format!("/proc/self/fd/{}", path_fd.as_raw_fd());
        let fd = openat(
            CWD,
            pinned.as_str(),
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let endpoint = Self {
            file: File::from(fd),
            block,
            size: target.size.as_u64(),
            dm_uuid,
        };
        endpoint.recheck()?;
        Ok(endpoint)
    }

    /// Rechecks the held descriptor and fresh kernel metadata before or after a native operation.
    pub fn recheck(&self) -> Result<(), EndpointError> {
        let info = fstat(&self.file).map_err(std::io::Error::from)?;
        let geometry = self.block.geometry.as_ref().ok_or(EndpointError::Changed)?;
        if FileType::from_raw_mode(info.st_mode) != FileType::BlockDevice
            || major(info.st_rdev) != self.block.devno.major
            || minor(info.st_rdev) != self.block.devno.minor
            || (&self.file).seek(SeekFrom::End(0))? != self.size
            || ioctl_blksszget(&self.file).map_err(std::io::Error::from)?
                != geometry.logical_block_size.get()
            || geometry
                .physical_block_size
                .as_ref()
                .is_some_and(|size| ioctl_blkpbszget(&self.file).ok() != Some(size.get()))
        {
            return Err(EndpointError::Changed);
        }
        let root = format!(
            "/sys/dev/block/{}:{}",
            self.block.devno.major, self.block.devno.minor
        );
        check_kernel_metadata(
            &std::fs::read_to_string(format!("{root}/dm/uuid"))?,
            &std::fs::read_to_string(format!("{root}/ro"))?,
            &std::fs::read_to_string(format!("{root}/size"))?,
            &self.dm_uuid,
            self.size,
        )
    }

    /// Borrows the retained file without exposing a replaceable descriptor or a display path.
    pub fn file(&self) -> &File {
        &self.file
    }
}

/// Rejects path traversal and proc-relative aliases before touching a candidate device inode.
fn normalized_device_path(path: &Path) -> bool {
    path.starts_with("/dev")
        && path.is_absolute()
        && !path.components().any(|part| part == Component::ParentDir)
        && !path.as_os_str().as_encoded_bytes().contains(&0)
}

/// Fixed-length native LVM IDs prevent ambiguous concatenation into the kernel DM identity.
fn expected_dm_uuid(vg: &str, lv: &str) -> Result<String, EndpointError> {
    let mut ids = Vec::new();
    for value in [vg, lv] {
        if value.contains('-')
            && value.split('-').map(str::len).collect::<Vec<_>>() != [6, 4, 4, 4, 4, 4, 6]
        {
            return Err(EndpointError::Changed);
        }
        let value = value.replace('-', "");
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(EndpointError::Changed);
        }
        ids.push(value);
    }
    Ok(format!("LVM-{}{}", ids[0], ids[1]))
}

/// The sysfs size unit is always 512 bytes; malformed or read-only metadata cannot pass preflight.
fn check_kernel_metadata(
    uuid: &str,
    read_only: &str,
    sectors: &str,
    expected: &str,
    bytes: u64,
) -> Result<(), EndpointError> {
    if uuid.trim_end_matches('\n') != expected
        || read_only.trim_end_matches('\n') != "0"
        || sectors
            .trim_end_matches('\n')
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(512))
            != Some(bytes)
    {
        return Err(EndpointError::Changed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Descriptor identity is constrained by both durable LVM IDs and exact writable kernel geometry.
    #[test]
    fn kernel_metadata_requires_exact_lv_identity_capacity_and_writable_state()
    -> Result<(), EndpointError> {
        let uuid = expected_dm_uuid(&"a".repeat(32), &"b".repeat(32))?;
        check_kernel_metadata(
            &format!("{uuid}\n"),
            "0\n",
            "32768\n",
            &uuid,
            16 * 1024 * 1024,
        )?;
        for (actual, ro, sectors) in [
            ("wrong", "0", "32768"),
            (&uuid, "1", "32768"),
            (&uuid, "", "32768"),
            (&uuid, "0", "32767"),
            (&uuid, "0", "18446744073709551615"),
        ] {
            assert!(check_kernel_metadata(actual, ro, sectors, &uuid, 16 * 1024 * 1024).is_err());
        }
        assert!(expected_dm_uuid("short", &"b".repeat(32)).is_err());
        for path in [
            "relative",
            "/dev/../etc/passwd",
            "/proc/self/fd/1",
            "/device/alias",
        ] {
            assert!(!normalized_device_path(Path::new(path)));
        }
        Ok(())
    }
    /// A character device is rejected through O_PATH inspection before any read-write device open.
    #[test]
    fn endpoint_rejects_non_block_devices_without_invocation()
    -> Result<(), Box<dyn std::error::Error>> {
        use storage_core::model::{BlockGeometry, BlockSize, Bytes, DeviceNumber, NodeId};
        let target = ResolvedLvTarget {
            planned_id: NodeId::new(),
            observed_id: NodeId::new(),
            lv_uuid: "b".repeat(32),
            lv_name: "data".into(),
            size: Bytes::new(16 * 1024 * 1024),
            block: Some(BlockFacts {
                paths: vec!["/dev/null".into()],
                devno: DeviceNumber { major: 1, minor: 3 },
                read_only: Some(false),
                geometry: Some(BlockGeometry {
                    logical_block_size: BlockSize::new(512).ok_or("block size")?,
                    physical_block_size: None,
                    alignment_offset: None,
                    minimum_io_size: None,
                    optimal_io_size: None,
                }),
            }),
        };
        assert!(matches!(
            LvBlockEndpoint::open(&target, &"a".repeat(32)),
            Err(EndpointError::Changed)
        ));
        Ok(())
    }
}
