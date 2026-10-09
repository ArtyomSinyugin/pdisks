#![deny(unsafe_code)]

//! Discovery of optional native libraries and command-line tools.
//!
//! Dynamic-library inspection is isolated in the FFI module. Provider-specific
//! calls belong to their own narrowly scoped FFI adapters.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

mod action;
mod contract;
mod dylib;
mod logical;
mod luks;
mod transport;

pub use action::{
    BtrfsAction, FilesystemAction, LvmAction, MountAction, PartitionAction, ProviderAction,
};
pub use contract::{
    ActionContract, ActionContractMismatch, ActionDescriptionContext, ActionOutput,
    ActionPrerequisite, ActionResource, FactInvalidation, FactKind, FactScope, GraphEffect,
    MountEffect, NodeKindClass, Postcondition, PredictedEffect, ResourceAccess, ResourceAccessMode,
    RestartPolicy, describe_action, verify_imported_contract,
};
pub use logical::{LogicalProvider, ProviderId};
pub use luks::{
    CredentialRef, CryptsetupBackend, LuksAction, LuksObservation, LuksProbeFailure,
    LuksProbeReport, LuksProvider, LuksValidationError,
};
pub use transport::{
    PROVIDER_PROTOCOL_VERSION, ProviderBinding, ProviderConnection, ProviderConnectionKind,
    ProviderRemoteError, ProviderRequest, ProviderResponse, ProviderResponsePayload,
    ProviderTransport, ProviderTransportError,
};

/// Manifest JSON schema version supported by this release.
pub const BACKEND_MANIFEST_VERSION: u16 = 1;

/// Environment variable overriding the backend manifest file.
///
/// The variable name is retained for compatibility with existing deployments.
pub const BACKEND_MANIFEST_ENV: &str = "PDISKS_PROVIDER_MANIFEST";

/// Provider manifest file used when no environment override is set.
pub const DEFAULT_BACKEND_MANIFEST: &str = "/usr/share/pdisks/providers.json";

/// Stable identity of a concrete system backend integration.
#[repr(transparent)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct BackendId(String);

impl BackendId {
    /// Creates an ID without whitespace.
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty() && !value.chars().any(char::is_whitespace)).then_some(Self(value))
    }

    /// Returns the opaque backend ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for BackendId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| {
            <D::Error as serde::de::Error>::custom(
                "backend ID must be non-empty and contain no whitespace",
            )
        })
    }
}

/// Coarse roles implemented by a concrete system backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendCapability {
    /// Read-only discovery.
    Probe,
    /// Technology-specific validation.
    Validate,
    /// Plan fragment construction.
    Plan,
    /// Privileged action execution.
    Execute,
}

/// Native library expected by a system backend integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendLibrary {
    /// Absolute path supplied by the target package build.
    pub path: PathBuf,
    /// Symbols required by the compiled pdisks adapter.
    #[serde(default)]
    pub required_symbols: BTreeSet<String>,
    /// Additional libraries consumed by the same typed adapter.
    #[serde(default)]
    pub auxiliary: Vec<AuxiliaryLibrary>,
}

/// Additional dynamic library retained by a multi-library backend adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuxiliaryLibrary {
    /// Absolute path supplied by the target package build.
    pub path: PathBuf,
    /// Symbols resolved from this auxiliary library.
    #[serde(default)]
    pub required_symbols: BTreeSet<String>,
}

/// Versioned project-owned collection of system backend integrations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendManifests {
    /// Version of this JSON structure, not of any native library.
    pub manifest_version: u16,
    /// Optional backend integrations known to this pdisks build.
    #[serde(rename = "providers")]
    pub backends: Vec<BackendManifest>,
}

/// Project-owned description of one optional system backend integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendManifest {
    /// Stable integration identity.
    pub id: BackendId,
    /// Preferred native library, when this integration has one.
    #[serde(default)]
    pub library: Option<BackendLibrary>,
    /// Fallback command-line tool, when this integration has one.
    #[serde(default)]
    pub executable: Option<PathBuf>,
    /// Roles exposed after a backend passes availability checks.
    pub capabilities: BTreeSet<BackendCapability>,
}

/// Mechanism used to call a concrete system backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendAccess {
    /// Native library loaded by a backend-specific C FFI adapter.
    Library,
    /// External command invoked by a backend-specific typed wrapper.
    Executable,
}

/// Available system backend registered for use by a logical provider.
#[derive(Debug)]
pub struct RegisteredBackend {
    manifest: BackendManifest,
    manifest_path: PathBuf,
    access: LoadedBackendAccess,
}

/// Resources retained for the selected system backend access mechanism.
#[derive(Debug)]
enum LoadedBackendAccess {
    /// Loaded native library kept alive for its future typed FFI adapter.
    Library {
        /// Handle retained so resolved symbols cannot outlive their library.
        library: dylib::LoadedLibrary,
    },
    /// Available command-line backend.
    Executable,
}

impl RegisteredBackend {
    /// Returns the validated backend descriptor.
    pub fn manifest(&self) -> &BackendManifest {
        &self.manifest
    }

    /// Returns the source manifest path.
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    /// Returns the mechanism used to access this backend on the host.
    pub fn access(&self) -> BackendAccess {
        match self.access {
            LoadedBackendAccess::Library { .. } => BackendAccess::Library,
            LoadedBackendAccess::Executable => BackendAccess::Executable,
        }
    }

    /// Reads the live mount table through a registered libmount backend.
    ///
    /// The backend manifest must select a dynamic library exporting the
    /// libmount symbols used by this adapter. CLI-backed providers are rejected.
    pub fn probe_libmount(&self) -> Result<Vec<LibmountEntry>, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_libmount(),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Enumerates live Linux block endpoints through libudev.
    pub fn probe_udev_blocks(&self) -> Result<Vec<UdevBlockEntry>, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_udev_blocks(),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads partition tables for the supplied whole-disk devices through libfdisk.
    pub fn probe_fdisk_partitions(
        &self,
        devices: &[PathBuf],
    ) -> Result<Vec<FdiskTable>, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_fdisk_partitions(devices),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads cached and freshly probed content signatures through libblkid.
    pub fn probe_blkid_signatures(&self) -> Result<Vec<BlkidEntry>, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_blkid_signatures(),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads live device-mapper names, targets, and backing devices.
    pub fn probe_devmapper(&self) -> Result<Vec<DevmapperEntry>, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_devmapper(),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads LUKS metadata from the supplied block devices through libcryptsetup.
    pub fn probe_cryptsetup(
        &self,
        devices: &[PathBuf],
    ) -> Result<Vec<CryptsetupEntry>, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_cryptsetup(devices),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads loop-device backing files through the libblockdev loop plugin.
    pub fn probe_loop(&self, devices: &[PathBuf]) -> Result<Vec<LoopEntry>, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_loop(devices),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads activation state for known swap devices through libblockdev.
    pub fn probe_swap(&self, devices: &[PathBuf]) -> Result<Vec<SwapEntry>, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_swap(devices),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads multipath member devices through the libblockdev mpath plugin.
    pub fn probe_multipath(&self) -> Result<MultipathEntry, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_multipath(),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads MD arrays and member superblocks through libblockdev mdraid.
    pub fn probe_mdraid(
        &self,
        arrays: &[PathBuf],
        members: &[PathBuf],
    ) -> Result<MdraidEntry, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_mdraid(arrays, members),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads LVM PV, VG, and LV topology through the libblockdev LVM plugin.
    pub fn probe_lvm(&self) -> Result<LvmEntry, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_lvm(),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads Btrfs subvolumes below mounted filesystem roots.
    pub fn probe_btrfs(
        &self,
        mountpoints: &[PathBuf],
    ) -> Result<Vec<BtrfsEntry>, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_btrfs(mountpoints),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads Btrfs filesystem and member-device topology through libblockdev.
    pub fn probe_btrfs_filesystems(
        &self,
        devices: &[PathBuf],
    ) -> Result<Vec<BtrfsFilesystemEntry>, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_btrfs_filesystems(devices),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reports operation groups available through libblockdev-btrfs.
    pub fn probe_btrfs_capabilities(&self) -> Result<BtrfsCapabilities, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_btrfs_capabilities(),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads imported ZFS pools and datasets through libzfs.
    pub fn probe_zfs(&self) -> Result<ZfsEntry, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_zfs(),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads the live subsystem, controller, and namespace tree through libnvme.
    pub fn probe_nvme(&self) -> Result<NvmeEntry, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_nvme(),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reports ordinary-filesystem operations exposed by libblockdev-fs.
    ///
    /// The result distinguishes support compiled into libblockdev from host
    /// availability, because many libblockdev filesystem operations delegate
    /// to a filesystem-specific utility installed on the host.
    pub fn probe_filesystem_capabilities(
        &self,
    ) -> Result<Vec<FilesystemCapabilities>, NativeProbeError> {
        match &self.access {
            LoadedBackendAccess::Library { library } => library.probe_filesystem_capabilities(),
            LoadedBackendAccess::Executable => Err(NativeProbeError::LibraryRequired),
        }
    }
}

/// Host availability of one filesystem operation exposed by libblockdev-fs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemOperationAvailability {
    /// Whether the operation can be performed on this host.
    pub available: bool,
    /// External utility required by libblockdev when the operation is unavailable.
    pub required_utility: Option<String>,
}

/// Resize directions supported by one filesystem implementation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FilesystemResizeCapabilities {
    /// Shrinking while unmounted is supported by the implementation.
    pub offline_shrink: bool,
    /// Growing while unmounted is supported by the implementation.
    pub offline_grow: bool,
    /// Shrinking while mounted is supported by the implementation.
    pub online_shrink: bool,
    /// Growing while mounted is supported by the implementation.
    pub online_grow: bool,
}

/// Creation options accepted by one filesystem implementation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FilesystemCreateOptions {
    /// A label can be assigned during creation.
    pub label: bool,
    /// A UUID can be assigned during creation.
    pub uuid: bool,
    /// The implementation provides a dry-run mode.
    pub dry_run: bool,
    /// Discard can be disabled during creation.
    pub no_discard: bool,
    /// A force flag is accepted.
    pub force: bool,
    /// Partition-table probing can be disabled.
    pub no_partition_table: bool,
}

/// Static and host-specific capabilities of one ordinary filesystem backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemCapabilities {
    /// Filesystem name accepted by libblockdev-fs.
    pub filesystem: String,
    /// Resize modes implemented for this filesystem.
    pub resize: FilesystemResizeCapabilities,
    /// Options accepted by the generic mkfs entrypoint.
    pub create_options: FilesystemCreateOptions,
    /// Minimum filesystem size reported by libblockdev.
    pub minimum_size: u64,
    /// Maximum filesystem size reported by libblockdev, or zero if unspecified.
    pub maximum_size: u64,
    /// Native partition identifier suggested for MBR.
    pub partition_id: Option<String>,
    /// Native partition type suggested for GPT.
    pub partition_type: Option<String>,
    /// Filesystem creation availability on this host.
    pub create: FilesystemOperationAvailability,
    /// Filesystem resize availability on this host.
    pub resize_operation: FilesystemOperationAvailability,
    /// Filesystem consistency-check availability on this host.
    pub check: FilesystemOperationAvailability,
    /// Filesystem repair availability on this host.
    pub repair: FilesystemOperationAvailability,
    /// Label modification availability on this host.
    pub set_label: FilesystemOperationAvailability,
    /// UUID modification availability on this host.
    pub set_uuid: FilesystemOperationAvailability,
    /// Filesystem size query availability on this host.
    pub get_size: FilesystemOperationAvailability,
    /// Free-space query availability on this host.
    pub get_free_space: FilesystemOperationAvailability,
    /// Minimum-size query availability on this host.
    pub get_minimum_size: FilesystemOperationAvailability,
}

/// One Linux block endpoint returned by libudev.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdevBlockEntry {
    /// Kernel device node.
    pub devnode: PathBuf,
    /// Stable and descriptive device links exported by udev.
    pub aliases: Vec<PathBuf>,
    /// Kernel device name.
    pub sysname: String,
    /// Udev device type, normally `disk` or `partition`.
    pub devtype: Option<String>,
    /// Kernel major device number.
    pub major: u32,
    /// Kernel minor device number.
    pub minor: u32,
    /// Device capacity in 512-byte sectors.
    pub size_sectors: Option<u64>,
    /// Logical block size in bytes.
    pub logical_block_size: Option<u32>,
    /// Physical block size in bytes.
    pub physical_block_size: Option<u32>,
    /// Required alignment offset in bytes.
    pub alignment_offset: Option<u64>,
    /// Minimum efficient I/O size in bytes.
    pub minimum_io_size: Option<u64>,
    /// Optimal I/O size in bytes.
    pub optimal_io_size: Option<u64>,
    /// Hardware model reported by udev.
    pub model: Option<String>,
    /// Hardware vendor reported by udev.
    pub vendor: Option<String>,
    /// Udev transport/bus identifier.
    pub transport: Option<String>,
    /// Hardware serial reported by udev.
    pub serial: Option<String>,
    /// World-wide name reported by udev.
    pub wwn: Option<String>,
    /// Kernel read-only flag.
    pub read_only: Option<bool>,
    /// Kernel rotational-media flag.
    pub rotational: Option<bool>,
    /// Kernel removable-media flag.
    pub removable: Option<bool>,
    /// Kernel zoned block-device model.
    pub zoned: Option<String>,
}

/// One partition table returned by libfdisk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdiskTable {
    /// Whole-disk device inspected by libfdisk.
    pub device: PathBuf,
    /// Native table label such as `gpt` or `dos`.
    pub label: Option<String>,
    /// Logical sector size used by partition offsets and sizes.
    pub sector_size: u64,
    /// Partitions present in the table.
    pub partitions: Vec<FdiskPartition>,
}

/// One partition returned by libfdisk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdiskPartition {
    /// One-based partition number.
    pub number: u32,
    /// Start offset in logical sectors.
    pub start_sectors: u64,
    /// Length in logical sectors.
    pub size_sectors: u64,
    /// Native partition type string.
    pub partition_type: Option<String>,
    /// Optional partition name.
    pub name: Option<String>,
    /// Optional partition UUID.
    pub uuid: Option<String>,
    /// Legacy bootable flag.
    pub bootable: bool,
}

/// One block-content signature returned by libblkid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlkidEntry {
    /// Device containing the signature.
    pub device: PathBuf,
    /// Signature type, for example `ext4`, `swap`, or `crypto_LUKS`.
    pub signature_type: Option<String>,
    /// Content UUID.
    pub uuid: Option<String>,
    /// Content label.
    pub label: Option<String>,
    /// Partition UUID known to blkid.
    pub partition_uuid: Option<String>,
}

/// One device number returned by a native topology provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NativeDeviceNumber {
    /// Kernel major device number.
    pub major: u32,
    /// Kernel minor device number.
    pub minor: u32,
}

/// One active device-mapper mapping returned by libdevmapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevmapperEntry {
    /// Stable kernel mapping name.
    pub name: String,
    /// Optional device-mapper UUID.
    pub uuid: Option<String>,
    /// Kernel device number of the mapped endpoint.
    pub devno: NativeDeviceNumber,
    /// Target types found in the live mapping table.
    pub targets: Vec<String>,
    /// Kernel device numbers referenced by the mapping.
    pub dependencies: Vec<NativeDeviceNumber>,
    /// Whether the mapping is read-only.
    pub read_only: bool,
}

/// LUKS header metadata returned by libcryptsetup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptsetupEntry {
    /// Block device containing the LUKS header.
    pub device: PathBuf,
    /// Loaded cryptsetup type, normally `LUKS1` or `LUKS2`.
    pub luks_type: String,
    /// Header UUID without interpretation.
    pub uuid: Option<String>,
    /// Encryption cipher name.
    pub cipher: Option<String>,
    /// Encryption cipher mode.
    pub cipher_mode: Option<String>,
    /// Data offset in 512-byte sectors as defined by libcryptsetup.
    pub data_offset_sectors: u64,
    /// Encryption sector size in bytes.
    pub sector_size: Option<u32>,
    /// Volume-key size in bytes.
    pub volume_key_size: Option<u32>,
    /// Metadata area size in bytes.
    pub metadata_size: Option<u64>,
    /// Keyslot area size in bytes.
    pub keyslots_size: Option<u64>,
    /// Non-inactive keyslots without any key material.
    pub keyslots: Vec<CryptsetupKeyslotEntry>,
}

/// Public state of one LUKS keyslot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CryptsetupKeyslotEntry {
    /// Zero-based keyslot index.
    pub index: u32,
    /// State reported by libcryptsetup.
    pub state: CryptsetupKeyslotState,
}

/// Non-inactive keyslot states exposed without reading secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptsetupKeyslotState {
    /// Keyslot contains a usable key.
    Active,
    /// Keyslot is the last active slot protecting the volume.
    ActiveLast,
    /// LUKS2 keyslot is not bound to a data-segment digest.
    Unbound,
    /// Provider state not recognized by this build.
    Other(i32),
}

/// Loop-device metadata returned by the libblockdev loop plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopEntry {
    /// Loop block-device path.
    pub device: PathBuf,
    /// File backing the loop device.
    pub backing_file: PathBuf,
    /// Byte offset into the backing file.
    pub offset: u64,
    /// Whether the kernel automatically clears the loop device.
    pub autoclear: bool,
    /// Whether direct I/O is active.
    pub direct_io: bool,
    /// Whether partition scanning is enabled.
    pub part_scan: bool,
    /// Whether the loop device is read-only.
    pub read_only: bool,
}

/// Runtime swap activation returned by the libblockdev swap plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapEntry {
    /// Swap block-device path.
    pub device: PathBuf,
    /// Whether the swap area is active.
    pub active: bool,
}

/// Multipath member set returned by the libblockdev mpath plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MultipathEntry {
    /// Paths currently recognized as multipath members.
    pub members: Vec<PathBuf>,
}

/// MD topology returned by the libblockdev mdraid plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MdraidEntry {
    /// Active MD arrays.
    pub arrays: Vec<MdraidArrayEntry>,
    /// Block devices carrying MD member metadata.
    pub members: Vec<MdraidMemberEntry>,
}

/// One active MD array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdraidArrayEntry {
    /// Array block-device path.
    pub device: PathBuf,
    /// MD personality string.
    pub level: String,
    /// Superblock metadata version.
    pub metadata: Option<String>,
    /// Array UUID.
    pub uuid: Option<String>,
    /// Array size in bytes.
    pub size: u64,
    /// Configured RAID device count.
    pub raid_devices: u64,
    /// Total devices currently associated with the array.
    pub total_devices: u64,
    /// Devices active in the array.
    pub active_devices: u64,
    /// Devices considered working by MD.
    pub working_devices: u64,
    /// Devices considered failed by MD.
    pub failed_devices: u64,
    /// Devices currently assigned as spares.
    pub spare_devices: u64,
    /// Whether MD reports the array as clean.
    pub clean: bool,
    /// Provider status string.
    pub status: Option<String>,
    /// Configured write-intent bitmap location.
    pub bitmap: Option<String>,
}

/// One block device carrying an MD member superblock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdraidMemberEntry {
    /// Member block-device path.
    pub device: PathBuf,
    /// MD personality string stored in the superblock.
    pub level: Option<String>,
    /// Array UUID stored in the superblock.
    pub array_uuid: Option<String>,
    /// Per-device UUID stored in the superblock.
    pub device_uuid: Option<String>,
    /// Superblock metadata version.
    pub metadata: Option<String>,
    /// Chunk size in bytes.
    pub chunk_size: u64,
    /// Number of devices expected by the member superblock.
    pub expected_devices: u64,
    /// Component size in bytes.
    pub size: u64,
    /// Last superblock update timestamp.
    pub update_time: u64,
    /// Superblock event counter used to compare freshness.
    pub events: u64,
}

/// LVM topology returned by the libblockdev LVM plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LvmEntry {
    /// Physical volumes.
    pub pvs: Vec<LvmPvEntry>,
    /// Volume groups.
    pub vgs: Vec<LvmVgEntry>,
    /// Logical volumes.
    pub lvs: Vec<LvmLvEntry>,
}

/// One LVM physical volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LvmPvEntry {
    /// Backing block-device path.
    pub device: PathBuf,
    /// PV UUID.
    pub uuid: String,
    /// Owning volume-group name, when assigned.
    pub vg_name: Option<String>,
    /// Owning volume-group UUID, when assigned.
    pub vg_uuid: Option<String>,
    /// PV size in bytes.
    pub size: u64,
    /// Unallocated bytes on the PV.
    pub free: u64,
    /// Byte offset of the first physical extent.
    pub data_offset: u64,
    /// Whether LVM reports the PV as missing.
    pub missing: bool,
}

/// One LVM volume group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LvmVgEntry {
    /// Volume-group name.
    pub name: String,
    /// VG UUID.
    pub uuid: String,
    /// VG size in bytes.
    pub size: u64,
    /// Physical extent size in bytes.
    pub extent_size: u64,
    /// Unallocated bytes in the volume group.
    pub free: u64,
    /// Number of physical volumes in the group.
    pub pv_count: u64,
    /// Whether the volume group is exported.
    pub exported: bool,
}

/// One LVM logical volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LvmLvEntry {
    /// Logical-volume name.
    pub name: String,
    /// Owning volume-group name.
    pub vg_name: String,
    /// LV UUID.
    pub uuid: String,
    /// LV size in bytes.
    pub size: u64,
    /// Provider segment type.
    pub segment_type: Option<String>,
    /// Native LVM attribute string.
    pub attributes: Option<String>,
    /// Origin LV name for snapshots.
    pub origin: Option<String>,
    /// Pool LV name for thin or cached volumes.
    pub pool: Option<String>,
    /// Data LV name used by a pool.
    pub data: Option<String>,
    /// Metadata LV name used by a pool.
    pub metadata: Option<String>,
    /// Provider role list.
    pub roles: Option<String>,
    /// Data usage scaled according to the LVM report API.
    pub data_percent: u64,
    /// Metadata usage scaled according to the LVM report API.
    pub metadata_percent: u64,
    /// Mirror or RAID copy progress scaled according to the LVM report API.
    pub copy_percent: u64,
    /// Physical allocation segments reported for the LV.
    pub segments: Vec<LvmSegmentEntry>,
}

/// One physical allocation segment of an LVM logical volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LvmSegmentEntry {
    /// Segment length in physical extents.
    pub size_extents: u64,
    /// Starting physical extent on the member PV.
    pub start_extent: u64,
    /// Physical-volume device hosting the segment.
    pub device: Option<PathBuf>,
}

/// Subvolumes observed below one mounted Btrfs filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsEntry {
    /// Mountpoint used for the read-only query.
    pub mountpoint: PathBuf,
    /// Subvolumes returned by libbtrfsutil.
    pub subvolumes: Vec<BtrfsSubvolumeEntry>,
}

/// One Btrfs subvolume returned by libbtrfsutil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsSubvolumeEntry {
    /// Filesystem-local subvolume ID.
    pub id: u64,
    /// Path relative to the Btrfs filesystem root.
    pub path: PathBuf,
    /// Whether the subvolume is read-only.
    pub read_only: bool,
    /// Whether this is the filesystem's default subvolume.
    pub is_default: bool,
}

/// Btrfs filesystem metadata returned by libblockdev-btrfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsFilesystemEntry {
    /// Device used to address the filesystem during discovery.
    pub seed_device: PathBuf,
    /// Filesystem UUID reported by Btrfs.
    pub uuid: String,
    /// Optional filesystem label.
    pub label: Option<String>,
    /// Number of devices recorded by the filesystem.
    pub device_count: u64,
    /// Bytes allocated by the filesystem.
    pub used: u64,
    /// Member devices visible to the provider.
    pub devices: Vec<BtrfsDeviceEntry>,
}

/// One device participating in a Btrfs filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsDeviceEntry {
    /// Btrfs-local device identifier.
    pub id: u64,
    /// Current block-device path.
    pub path: PathBuf,
    /// Device capacity in bytes.
    pub size: u64,
    /// Bytes allocated on this member.
    pub used: u64,
}

/// Operation groups currently available through libblockdev-btrfs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BtrfsCapabilities {
    /// Basic filesystem queries and modifications are available.
    pub filesystem: bool,
    /// Multi-device creation and member changes are available.
    pub multi_device: bool,
    /// Subvolume creation, deletion, and default selection are available.
    pub subvolume: bool,
    /// Snapshot creation and deletion are available.
    pub snapshot: bool,
}

/// Imported ZFS topology returned by libzfs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZfsEntry {
    /// Imported storage pools.
    pub pools: Vec<ZfsPoolEntry>,
    /// Filesystems and volumes visible below imported pools.
    pub datasets: Vec<ZfsDatasetEntry>,
}

/// One imported ZFS pool and its virtual-device tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZfsPoolEntry {
    /// Canonical pool name.
    pub name: String,
    /// Stable pool GUID.
    pub guid: Option<u64>,
    /// Native pool state value returned by libzfs.
    pub state: i32,
    /// Flattened virtual-device tree with parent identities.
    pub vdevs: Vec<ZfsVdevEntry>,
}

/// One ZFS virtual device copied from the pool configuration nvlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZfsVdevEntry {
    /// Stable vdev GUID when present in the pool configuration.
    pub guid: Option<u64>,
    /// Parent vdev GUID, or `None` for a top-level allocation vdev.
    pub parent_guid: Option<u64>,
    /// Allocation class used when this is a top-level vdev.
    pub class: Option<ZfsVdevClass>,
    /// Vdev layout.
    pub kind: ZfsVdevKind,
    /// Backing path for leaf devices.
    pub path: Option<PathBuf>,
    /// Allocated size reported by the pool configuration.
    pub size: Option<u64>,
    /// Whether the vdev is currently marked missing.
    pub missing: bool,
    /// Whether the vdev is currently marked faulted.
    pub faulted: bool,
    /// Whether the vdev is currently marked degraded.
    pub degraded: bool,
}

/// ZFS virtual-device layouts represented by the canonical storage model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZfsVdevKind {
    /// One disk or file leaf.
    Leaf,
    /// Mirrored children.
    Mirror,
    /// RAIDZ with the supplied parity width.
    RaidZ { parity: u8 },
    /// Distributed RAID layout.
    DRaid {
        /// Parity width.
        parity: u8,
        /// Data-device width.
        data_width: u32,
        /// Distributed spare count.
        distributed_spares: u16,
    },
    /// Provider layout not represented by a dedicated variant.
    Other(String),
}

/// Allocation class of a top-level ZFS vdev.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZfsVdevClass {
    /// Ordinary data allocation.
    Data,
    /// ZFS intent log.
    Log,
    /// Special allocation class.
    Special,
    /// Deduplication-table allocation class.
    Dedup,
    /// Level-two ARC cache.
    Cache,
    /// Hot spare.
    Spare,
}

/// One ZFS filesystem or volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZfsDatasetEntry {
    /// Canonical ZFS dataset name.
    pub name: String,
    /// Dataset object type.
    pub kind: ZfsDatasetKind,
    /// Effective mountpoint property when returned by libzfs_core.
    pub mountpoint: Option<String>,
    /// Effective compression property when returned by libzfs_core.
    pub compression: Option<String>,
    /// Effective quota in bytes, with zero representing no quota.
    pub quota: Option<u64>,
    /// Zvol size in bytes.
    pub volume_size: Option<u64>,
    /// Snapshot origin of a clone.
    pub origin: Option<String>,
}

/// ZFS object kinds represented by the canonical model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZfsDatasetKind {
    /// Mountable ZFS filesystem.
    Filesystem,
    /// Block-device ZFS volume.
    Volume,
}

/// Live NVMe subsystem topology returned by libnvme.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NvmeEntry {
    /// NVMe subsystems visible to the host.
    pub subsystems: Vec<NvmeSubsystemEntry>,
}

/// One NVMe subsystem with its controllers and namespaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmeSubsystemEntry {
    /// Kernel subsystem name.
    pub name: String,
    /// NVMe Qualified Name.
    pub nqn: String,
    /// Controllers attached to the subsystem.
    pub controllers: Vec<NvmeControllerEntry>,
    /// Namespaces exported by the subsystem.
    pub namespaces: Vec<NvmeNamespaceEntry>,
}

/// One NVMe controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmeControllerEntry {
    /// Kernel controller name.
    pub name: String,
    /// libnvme transport string such as `pcie` or `tcp`.
    pub transport: Option<String>,
    /// Kernel controller address string.
    pub address: Option<String>,
    /// NVMe-oF transport address.
    pub transport_address: Option<String>,
    /// NVMe-oF transport service identifier.
    pub transport_service_id: Option<String>,
    /// Controller model.
    pub model: Option<String>,
    /// Controller serial number.
    pub serial: Option<String>,
    /// Controller firmware revision.
    pub firmware: Option<String>,
    /// Kernel controller state.
    pub state: Option<String>,
}

/// One NVMe namespace and its active block format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmeNamespaceEntry {
    /// Kernel namespace name used below `/dev`.
    pub name: String,
    /// Non-zero namespace identifier.
    pub nsid: u32,
    /// Active logical block size in bytes.
    pub lba_size: u32,
    /// Metadata bytes stored with each logical block.
    pub metadata_size: u16,
    /// Number of logical blocks provided by the namespace.
    pub lba_count: u64,
    /// Number of logical blocks currently allocated or used.
    pub lba_utilization: u64,
    /// Namespace globally unique identifier, when non-zero.
    pub nguid: Option<[u8; 16]>,
    /// Namespace EUI-64 identifier, when non-zero.
    pub eui64: Option<[u8; 8]>,
    /// Namespace UUID bytes, when non-zero.
    pub uuid: Option<[u8; 16]>,
}

/// One runtime mount entry returned by the native libmount adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibmountEntry {
    /// Mount source exactly as exposed by libmount.
    pub source: OsString,
    /// Mount target exactly as exposed by libmount.
    pub target: PathBuf,
    /// Kernel filesystem type.
    pub filesystem_type: String,
    /// Parsed mount options.
    pub options: Vec<String>,
}

/// Failure while calling a backend-specific dynamic-library adapter.
#[derive(Debug, Error)]
pub enum NativeProbeError {
    /// The registered backend selected an executable instead of a library.
    #[error("native backend library is required")]
    LibraryRequired,
    /// A required symbol was absent from the loaded backend library.
    #[error("backend library does not export {symbol}")]
    MissingSymbol {
        /// Missing C symbol name.
        symbol: &'static str,
    },
    /// The native backend returned an error code.
    #[error("native backend call {operation} failed with code {code}")]
    CallFailed {
        /// Native operation that failed.
        operation: &'static str,
        /// Provider-specific return code.
        code: i32,
    },
    /// The native backend could not allocate a required object.
    #[error("native backend could not allocate {object}")]
    AllocationFailed {
        /// Native object whose constructor returned null.
        object: &'static str,
    },
    /// A device path contained an interior NUL byte and cannot cross C FFI.
    #[error("native backend cannot inspect path containing NUL: {path:?}")]
    InvalidPath {
        /// Rejected device path.
        path: PathBuf,
    },
    /// A native backend returned structurally invalid data.
    #[error("native backend returned invalid data from {operation}")]
    InvalidData {
        /// Native operation whose result violated its public ABI contract.
        operation: &'static str,
    },
}

/// Deterministic registry containing only integrations available on this host.
#[derive(Debug, Default)]
pub struct BackendRegistry {
    backends: BTreeMap<BackendId, RegisteredBackend>,
}

impl BackendRegistry {
    /// Loads manifests from the environment override or the system file.
    pub fn load() -> Result<Self, BackendRegistryError> {
        let path = std::env::var_os(BACKEND_MANIFEST_ENV)
            .map_or_else(|| PathBuf::from(DEFAULT_BACKEND_MANIFEST), PathBuf::from);
        Self::load_from(path)
    }

    /// Loads all backend declarations from one JSON file.
    ///
    /// A well-formed backend whose system dependency is unavailable on the
    /// host is skipped. Optional system packages therefore do not prevent startup.
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, BackendRegistryError> {
        let path = path.as_ref();
        let manifests = read_manifests(path)?;
        let mut registry = Self::default();
        for manifest in manifests.backends {
            registry.register(path, manifest)?;
        }
        Ok(registry)
    }

    /// Strictly verifies every backend declared for RPM `BuildRequires` tests.
    ///
    /// Unlike runtime discovery, this fails when any declared optional
    /// dependency is unavailable or lacks its required symbols.
    pub fn validate_dependencies(path: impl AsRef<Path>) -> Result<(), BackendRegistryError> {
        let path = path.as_ref();
        for manifest in read_manifests(path)?.backends {
            validate_manifest(path, &manifest)?;
            if let Some(library) = &manifest.library
                && dylib::LoadedLibrary::load(library).is_none()
            {
                return Err(BackendRegistryError::UnavailableLibrary {
                    backend: manifest.id,
                    library: library.path.clone(),
                });
            }
            if let Some(executable) = &manifest.executable
                && !is_executable_file(executable)
            {
                return Err(BackendRegistryError::UnavailableExecutable {
                    backend: manifest.id,
                    executable: executable.clone(),
                });
            }
        }
        Ok(())
    }

    /// Returns an available backend by stable ID.
    pub fn get(&self, id: &BackendId) -> Option<&RegisteredBackend> {
        self.backends.get(id)
    }

    /// Iterates over available backends in deterministic ID order.
    pub fn backends(&self) -> impl ExactSizeIterator<Item = &RegisteredBackend> {
        self.backends.values()
    }

    /// Validates and conditionally registers one backend declaration.
    fn register(
        &mut self,
        path: &Path,
        manifest: BackendManifest,
    ) -> Result<(), BackendRegistryError> {
        validate_manifest(path, &manifest)?;

        let Some(access) = available_backend_access(&manifest) else {
            return Ok(());
        };
        let id = manifest.id.clone();
        if self.backends.contains_key(&id) {
            return Err(BackendRegistryError::DuplicateBackend(id));
        }
        self.backends.insert(
            id,
            RegisteredBackend {
                manifest,
                manifest_path: path.to_path_buf(),
                access,
            },
        );
        Ok(())
    }
}

/// Reads and validates the versioned backend collection.
fn read_manifests(path: &Path) -> Result<BackendManifests, BackendRegistryError> {
    let bytes = fs::read(path).map_err(|source| BackendRegistryError::ReadManifest {
        path: path.to_path_buf(),
        source,
    })?;
    let manifests: BackendManifests =
        serde_json::from_slice(&bytes).map_err(|source| BackendRegistryError::ParseManifest {
            path: path.to_path_buf(),
            source,
        })?;
    if manifests.manifest_version != BACKEND_MANIFEST_VERSION {
        return Err(BackendRegistryError::UnsupportedManifestVersion {
            path: path.to_path_buf(),
            version: manifests.manifest_version,
        });
    }
    Ok(manifests)
}

/// Validates fields that cross the filesystem and dynamic-loader boundary.
fn validate_manifest(path: &Path, manifest: &BackendManifest) -> Result<(), BackendRegistryError> {
    if manifest.library.is_none() && manifest.executable.is_none() {
        return Err(BackendRegistryError::MissingBackend {
            path: path.to_path_buf(),
        });
    }
    if let Some(library) = &manifest.library {
        validate_library_fields(path, &library.path, &library.required_symbols)?;
        for auxiliary in &library.auxiliary {
            validate_library_fields(path, &auxiliary.path, &auxiliary.required_symbols)?;
        }
    }
    if let Some(executable) = &manifest.executable
        && !executable.is_absolute()
    {
        return Err(BackendRegistryError::RelativeExecutable {
            path: path.to_path_buf(),
            executable: executable.clone(),
        });
    }
    Ok(())
}

/// Validates one main or auxiliary dynamic-library declaration.
fn validate_library_fields(
    manifest_path: &Path,
    library_path: &Path,
    symbols: &BTreeSet<String>,
) -> Result<(), BackendRegistryError> {
    if !library_path.is_absolute() {
        return Err(BackendRegistryError::RelativeLibrary {
            path: manifest_path.to_path_buf(),
            library: library_path.to_path_buf(),
        });
    }
    if let Some(symbol) = symbols.iter().find(|symbol| symbol.as_bytes().contains(&0)) {
        return Err(BackendRegistryError::InvalidLibrarySymbol {
            path: manifest_path.to_path_buf(),
            symbol: symbol.clone(),
        });
    }
    Ok(())
}

/// Chooses the preferred backend that is actually available on this host.
fn available_backend_access(manifest: &BackendManifest) -> Option<LoadedBackendAccess> {
    if let Some(library) = manifest
        .library
        .as_ref()
        .and_then(dylib::LoadedLibrary::load)
    {
        Some(LoadedBackendAccess::Library { library })
    } else if manifest
        .executable
        .as_ref()
        .is_some_and(|path| is_executable_file(path))
    {
        Some(LoadedBackendAccess::Executable)
    } else {
        None
    }
}

/// Reports whether a path names an executable regular file.
fn is_executable_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Backend manifest discovery or validation failure.
#[derive(Debug, Error)]
pub enum BackendRegistryError {
    /// Manifest file could not be read.
    #[error("cannot read backend manifest {path:?}: {source}")]
    ReadManifest {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Manifest JSON did not match the strict schema.
    #[error("cannot parse backend manifest {path:?}: {source}")]
    ParseManifest {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// Manifest uses a JSON schema unknown to this release.
    #[error("backend manifest {path:?} uses unsupported schema version {version}")]
    UnsupportedManifestVersion { path: PathBuf, version: u16 },
    /// A library selected for an RPM build is missing or ABI-incompatible.
    #[error("backend {backend:?} cannot load library {library:?} with its required symbols")]
    UnavailableLibrary {
        backend: BackendId,
        library: PathBuf,
    },
    /// A command selected for an RPM build is missing or not executable.
    #[error("backend {backend:?} has unavailable executable {executable:?}")]
    UnavailableExecutable {
        backend: BackendId,
        executable: PathBuf,
    },
    /// Manifest declares neither a library nor a command fallback.
    #[error("backend manifest {path:?} declares no system access")]
    MissingBackend { path: PathBuf },
    /// Relative library paths would make privileged loading process-dependent.
    #[error("backend manifest {path:?} has relative library {library:?}")]
    RelativeLibrary { path: PathBuf, library: PathBuf },
    /// A symbol name cannot be passed to the native loader.
    #[error("backend manifest {path:?} has invalid library symbol {symbol:?}")]
    InvalidLibrarySymbol { path: PathBuf, symbol: String },
    /// Relative executable paths would make invocation process-dependent.
    #[error("backend manifest {path:?} has relative executable {executable:?}")]
    RelativeExecutable { path: PathBuf, executable: PathBuf },
    /// More than one available manifest declared the same ID.
    #[error("duplicate backend ID {0:?}")]
    DuplicateBackend(BackendId),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    /// Creates an isolated directory for one registry fixture.
    fn fixture_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pdisks-provider-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap_or_else(|error| panic!("create fixture: {error}"));
        path
    }

    /// Builds the smallest command-backed provider manifest.
    fn manifest(id: &str, executable: &str) -> BackendManifest {
        BackendManifest {
            id: BackendId::new(id).unwrap_or_else(|| unreachable!()),
            library: None,
            executable: Some(PathBuf::from(executable)),
            capabilities: BTreeSet::from([BackendCapability::Probe]),
        }
    }

    /// Writes one versioned provider collection fixture.
    fn write_manifests(directory: &Path, manifests: BackendManifests) -> PathBuf {
        let path = directory.join("providers.json");
        let json = serde_json::to_vec(&manifests)
            .unwrap_or_else(|error| panic!("serialize manifest fixture: {error}"));
        fs::write(&path, json).unwrap_or_else(|error| panic!("write fixture: {error}"));
        path
    }

    /// Wraps provider declarations in the current file schema.
    fn manifests(backends: Vec<BackendManifest>) -> BackendManifests {
        BackendManifests {
            manifest_version: BACKEND_MANIFEST_VERSION,
            backends,
        }
    }

    #[test]
    /// Verifies stable registry order regardless of declaration order.
    fn registry_discovers_available_providers_deterministically() {
        let directory = fixture_dir();
        let path = write_manifests(
            &directory,
            manifests(vec![
                manifest("zfs", "/bin/true"),
                manifest("block", "/bin/true"),
            ]),
        );

        let registry = BackendRegistry::load_from(path)
            .unwrap_or_else(|error| panic!("load providers: {error}"));
        BackendRegistry::validate_dependencies(directory.join("providers.json"))
            .unwrap_or_else(|error| panic!("validate provider dependencies: {error}"));
        assert_eq!(
            registry
                .backends()
                .map(|provider| provider.manifest().id.as_str())
                .collect::<Vec<_>>(),
            ["block", "zfs"]
        );
        fs::remove_dir_all(directory).unwrap_or_else(|error| panic!("remove fixture: {error}"));
    }

    #[test]
    /// Verifies that an absent optional dependency does not register capabilities.
    fn registry_skips_unavailable_provider() {
        let directory = fixture_dir();
        let path = write_manifests(
            &directory,
            manifests(vec![manifest("zfs", "/definitely/missing/zfs")]),
        );

        let registry = BackendRegistry::load_from(path)
            .unwrap_or_else(|error| panic!("load providers: {error}"));
        assert_eq!(registry.backends().len(), 0);
        assert!(matches!(
            BackendRegistry::validate_dependencies(directory.join("providers.json")),
            Err(BackendRegistryError::UnavailableExecutable { .. })
        ));
        fs::remove_dir_all(directory).unwrap_or_else(|error| panic!("remove fixture: {error}"));
    }

    #[test]
    /// Verifies that the fixed version belongs to the manifest schema only.
    fn registry_rejects_unsupported_manifest_schema() {
        let directory = fixture_dir();
        let mut incompatible = manifests(vec![manifest("block", "/bin/true")]);
        incompatible.manifest_version = BACKEND_MANIFEST_VERSION + 1;
        let path = write_manifests(&directory, incompatible);

        assert!(matches!(
            BackendRegistry::load_from(path),
            Err(BackendRegistryError::UnsupportedManifestVersion { version, .. })
                if version == BACKEND_MANIFEST_VERSION + 1
        ));
        fs::remove_dir_all(directory).unwrap_or_else(|error| panic!("remove fixture: {error}"));
    }

    #[test]
    /// Verifies trust-boundary validation for backend paths and symbol names.
    fn registry_rejects_invalid_backend_metadata() {
        let directory = fixture_dir();
        let mut relative = manifest("block", "bin/true");
        let path = write_manifests(&directory, manifests(vec![relative.clone()]));
        assert!(matches!(
            BackendRegistry::load_from(&path),
            Err(BackendRegistryError::RelativeExecutable { .. })
        ));

        relative.executable = None;
        relative.library = Some(BackendLibrary {
            path: PathBuf::from("/usr/lib64/libblock.so"),
            required_symbols: BTreeSet::from([String::from("bad\0symbol")]),
            auxiliary: Vec::new(),
        });
        write_manifests(&directory, manifests(vec![relative]));
        assert!(matches!(
            BackendRegistry::load_from(path),
            Err(BackendRegistryError::InvalidLibrarySymbol { .. })
        ));

        let mut relative_auxiliary = manifest("block", "/bin/true");
        relative_auxiliary.executable = None;
        relative_auxiliary.library = Some(BackendLibrary {
            path: PathBuf::from("/usr/lib64/libblock.so"),
            required_symbols: BTreeSet::new(),
            auxiliary: vec![AuxiliaryLibrary {
                path: PathBuf::from("libhelper.so"),
                required_symbols: BTreeSet::new(),
            }],
        });
        let path = write_manifests(&directory, manifests(vec![relative_auxiliary]));
        assert!(matches!(
            BackendRegistry::load_from(path),
            Err(BackendRegistryError::RelativeLibrary { .. })
        ));
        fs::remove_dir_all(directory).unwrap_or_else(|error| panic!("remove fixture: {error}"));
    }

    /// Verifies that JSON cannot bypass backend-ID constructor invariants.
    #[test]
    fn backend_id_rejects_invalid_json() {
        assert!(serde_json::from_str::<BackendId>(r#""""#).is_err());
        assert!(serde_json::from_str::<BackendId>(r#""two words""#).is_err());
    }
}
