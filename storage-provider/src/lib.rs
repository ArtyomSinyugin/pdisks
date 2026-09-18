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

mod dylib;

/// Manifest JSON schema version supported by this release.
pub const PROVIDER_MANIFEST_VERSION: u16 = 1;

/// Environment variable overriding the provider manifest file.
pub const PROVIDER_MANIFEST_ENV: &str = "PDISKS_PROVIDER_MANIFEST";

/// Provider manifest file used when no environment override is set.
pub const DEFAULT_PROVIDER_MANIFEST: &str = "/usr/share/pdisks/providers.json";

/// Stable identity of a provider integration.
#[repr(transparent)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    /// Creates an ID without whitespace.
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty() && !value.chars().any(char::is_whitespace)).then_some(Self(value))
    }

    /// Returns the opaque provider ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Independent roles implemented by a provider integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    /// Read-only discovery.
    Probe,
    /// Technology-specific validation.
    Validate,
    /// Plan fragment construction.
    Plan,
    /// Privileged action execution.
    Execute,
}

/// Native library expected by a provider integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderLibrary {
    /// Absolute path supplied by the target package build.
    pub path: PathBuf,
    /// Symbols required by the compiled pdisks adapter.
    #[serde(default)]
    pub required_symbols: BTreeSet<String>,
}

/// Versioned project-owned collection of provider integrations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderManifests {
    /// Version of this JSON structure, not of any native library.
    pub manifest_version: u16,
    /// Optional integrations known to this pdisks build.
    pub providers: Vec<ProviderManifest>,
}

/// Project-owned description of one optional provider integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderManifest {
    /// Stable integration identity.
    pub id: ProviderId,
    /// Preferred native library, when this integration has one.
    #[serde(default)]
    pub library: Option<ProviderLibrary>,
    /// Fallback command-line tool, when this integration has one.
    #[serde(default)]
    pub executable: Option<PathBuf>,
    /// Roles exposed after a backend passes availability checks.
    pub capabilities: BTreeSet<ProviderCapability>,
}

/// Available backend selected for a provider integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderBackend {
    /// Native library loaded by a provider-specific FFI adapter.
    Library,
    /// External command invoked by a provider-specific parser.
    Cli,
}

/// Available provider integration registered for later initialization.
#[derive(Debug)]
pub struct RegisteredProvider {
    manifest: ProviderManifest,
    manifest_path: PathBuf,
    backend: LoadedBackend,
}

/// Resources retained for the selected provider backend.
#[derive(Debug)]
enum LoadedBackend {
    /// Loaded native library kept alive for its future typed FFI adapter.
    Library {
        /// Handle retained so resolved symbols cannot outlive their library.
        library: dylib::LoadedLibrary,
    },
    /// Available command-line backend.
    Cli,
}

impl RegisteredProvider {
    /// Returns the validated provider descriptor.
    pub fn manifest(&self) -> &ProviderManifest {
        &self.manifest
    }

    /// Returns the source manifest path.
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    /// Returns the backend found on this host.
    pub fn backend(&self) -> ProviderBackend {
        match self.backend {
            LoadedBackend::Library { .. } => ProviderBackend::Library,
            LoadedBackend::Cli => ProviderBackend::Cli,
        }
    }

    /// Reads the live mount table through a registered libmount backend.
    ///
    /// The provider manifest must select a dynamic library exporting the
    /// libmount symbols used by this adapter. CLI-backed providers are rejected.
    pub fn probe_libmount(&self) -> Result<Vec<LibmountEntry>, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_libmount(),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Enumerates live Linux block endpoints through libudev.
    pub fn probe_udev_blocks(&self) -> Result<Vec<UdevBlockEntry>, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_udev_blocks(),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads partition tables for the supplied whole-disk devices through libfdisk.
    pub fn probe_fdisk_partitions(
        &self,
        devices: &[PathBuf],
    ) -> Result<Vec<FdiskTable>, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_fdisk_partitions(devices),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads cached and freshly probed content signatures through libblkid.
    pub fn probe_blkid_signatures(&self) -> Result<Vec<BlkidEntry>, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_blkid_signatures(),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads live device-mapper names, targets, and backing devices.
    pub fn probe_devmapper(&self) -> Result<Vec<DevmapperEntry>, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_devmapper(),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads LUKS metadata from the supplied block devices through libcryptsetup.
    pub fn probe_cryptsetup(
        &self,
        devices: &[PathBuf],
    ) -> Result<Vec<CryptsetupEntry>, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_cryptsetup(devices),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads loop-device backing files through the libblockdev loop plugin.
    pub fn probe_loop(&self, devices: &[PathBuf]) -> Result<Vec<LoopEntry>, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_loop(devices),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads activation state for known swap devices through libblockdev.
    pub fn probe_swap(&self, devices: &[PathBuf]) -> Result<Vec<SwapEntry>, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_swap(devices),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads multipath member devices through the libblockdev mpath plugin.
    pub fn probe_multipath(&self) -> Result<MultipathEntry, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_multipath(),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads MD arrays and member superblocks through libblockdev mdraid.
    pub fn probe_mdraid(
        &self,
        arrays: &[PathBuf],
        members: &[PathBuf],
    ) -> Result<MdraidEntry, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_mdraid(arrays, members),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads LVM PV, VG, and LV topology through the libblockdev LVM plugin.
    pub fn probe_lvm(&self) -> Result<LvmEntry, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_lvm(),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads Btrfs subvolumes below mounted filesystem roots.
    pub fn probe_btrfs(
        &self,
        mountpoints: &[PathBuf],
    ) -> Result<Vec<BtrfsEntry>, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_btrfs(mountpoints),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads imported ZFS pools and datasets through libzfs.
    pub fn probe_zfs(&self) -> Result<ZfsEntry, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_zfs(),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }

    /// Reads the live subsystem, controller, and namespace tree through libnvme.
    pub fn probe_nvme(&self) -> Result<NvmeEntry, NativeProbeError> {
        match &self.backend {
            LoadedBackend::Library { library } => library.probe_nvme(),
            LoadedBackend::Cli => Err(NativeProbeError::LibraryRequired),
        }
    }
}

/// One Linux block endpoint returned by libudev.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdevBlockEntry {
    /// Kernel device node.
    pub devnode: PathBuf,
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
    /// Hardware model reported by udev.
    pub model: Option<String>,
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

/// Imported ZFS topology returned by libzfs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZfsEntry {
    /// Imported storage pools.
    pub pools: Vec<String>,
    /// Filesystems and volumes visible below imported pools.
    pub datasets: Vec<ZfsDatasetEntry>,
}

/// One ZFS filesystem or volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZfsDatasetEntry {
    /// Canonical ZFS dataset name.
    pub name: String,
    /// Dataset object type.
    pub kind: ZfsDatasetKind,
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

/// Failure while calling a provider-specific dynamic-library adapter.
#[derive(Debug, Error)]
pub enum NativeProbeError {
    /// The registered provider selected a CLI backend instead of a library.
    #[error("native provider library is required")]
    LibraryRequired,
    /// A required symbol was absent from the loaded provider library.
    #[error("provider library does not export {symbol}")]
    MissingSymbol {
        /// Missing C symbol name.
        symbol: &'static str,
    },
    /// The native provider returned an error code.
    #[error("native provider call {operation} failed with code {code}")]
    CallFailed {
        /// Native operation that failed.
        operation: &'static str,
        /// Provider-specific return code.
        code: i32,
    },
    /// The native provider could not allocate a required object.
    #[error("native provider could not allocate {object}")]
    AllocationFailed {
        /// Native object whose constructor returned null.
        object: &'static str,
    },
    /// A device path contained an interior NUL byte and cannot cross C FFI.
    #[error("native provider cannot inspect path containing NUL: {path:?}")]
    InvalidPath {
        /// Rejected device path.
        path: PathBuf,
    },
    /// A native provider returned structurally invalid data.
    #[error("native provider returned invalid data from {operation}")]
    InvalidData {
        /// Native operation whose result violated its public ABI contract.
        operation: &'static str,
    },
}

/// Deterministic registry containing only integrations available on this host.
#[derive(Debug, Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<ProviderId, RegisteredProvider>,
}

impl ProviderRegistry {
    /// Loads manifests from the environment override or the system file.
    pub fn load() -> Result<Self, ProviderError> {
        let path = std::env::var_os(PROVIDER_MANIFEST_ENV)
            .map_or_else(|| PathBuf::from(DEFAULT_PROVIDER_MANIFEST), PathBuf::from);
        Self::load_from(path)
    }

    /// Loads all provider declarations from one JSON file.
    ///
    /// A well-formed provider whose declared backends are unavailable on the
    /// host is skipped. Optional system packages therefore do not prevent startup.
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, ProviderError> {
        let path = path.as_ref();
        let manifests = read_manifests(path)?;
        let mut registry = Self::default();
        for manifest in manifests.providers {
            registry.register(path, manifest)?;
        }
        Ok(registry)
    }

    /// Strictly verifies every backend declared for RPM `BuildRequires` tests.
    ///
    /// Unlike runtime discovery, this fails when any declared optional
    /// dependency is unavailable or lacks its required symbols.
    pub fn validate_dependencies(path: impl AsRef<Path>) -> Result<(), ProviderError> {
        let path = path.as_ref();
        for manifest in read_manifests(path)?.providers {
            validate_manifest(path, &manifest)?;
            if let Some(library) = &manifest.library
                && dylib::LoadedLibrary::load(library).is_none()
            {
                return Err(ProviderError::UnavailableLibrary {
                    provider: manifest.id,
                    library: library.path.clone(),
                });
            }
            if let Some(executable) = &manifest.executable
                && !is_executable_file(executable)
            {
                return Err(ProviderError::UnavailableExecutable {
                    provider: manifest.id,
                    executable: executable.clone(),
                });
            }
        }
        Ok(())
    }

    /// Returns an available provider by stable ID.
    pub fn get(&self, id: &ProviderId) -> Option<&RegisteredProvider> {
        self.providers.get(id)
    }

    /// Iterates over available providers in deterministic ID order.
    pub fn providers(&self) -> impl ExactSizeIterator<Item = &RegisteredProvider> {
        self.providers.values()
    }

    /// Validates and conditionally registers one provider declaration.
    fn register(&mut self, path: &Path, manifest: ProviderManifest) -> Result<(), ProviderError> {
        validate_manifest(path, &manifest)?;

        let Some(backend) = available_backend(&manifest) else {
            return Ok(());
        };
        let id = manifest.id.clone();
        if self.providers.contains_key(&id) {
            return Err(ProviderError::DuplicateProvider(id));
        }
        self.providers.insert(
            id,
            RegisteredProvider {
                manifest,
                manifest_path: path.to_path_buf(),
                backend,
            },
        );
        Ok(())
    }
}

/// Reads and validates the versioned provider collection.
fn read_manifests(path: &Path) -> Result<ProviderManifests, ProviderError> {
    let bytes = fs::read(path).map_err(|source| ProviderError::ReadManifest {
        path: path.to_path_buf(),
        source,
    })?;
    let manifests: ProviderManifests =
        serde_json::from_slice(&bytes).map_err(|source| ProviderError::ParseManifest {
            path: path.to_path_buf(),
            source,
        })?;
    if manifests.manifest_version != PROVIDER_MANIFEST_VERSION {
        return Err(ProviderError::UnsupportedManifestVersion {
            path: path.to_path_buf(),
            version: manifests.manifest_version,
        });
    }
    Ok(manifests)
}

/// Validates fields that cross the filesystem and dynamic-loader boundary.
fn validate_manifest(path: &Path, manifest: &ProviderManifest) -> Result<(), ProviderError> {
    if manifest.library.is_none() && manifest.executable.is_none() {
        return Err(ProviderError::MissingBackend {
            path: path.to_path_buf(),
        });
    }
    if let Some(library) = &manifest.library {
        if !library.path.is_absolute() {
            return Err(ProviderError::RelativeLibrary {
                path: path.to_path_buf(),
                library: library.path.clone(),
            });
        }
        if let Some(symbol) = library
            .required_symbols
            .iter()
            .find(|symbol| symbol.as_bytes().contains(&0))
        {
            return Err(ProviderError::InvalidLibrarySymbol {
                path: path.to_path_buf(),
                symbol: symbol.clone(),
            });
        }
    }
    if let Some(executable) = &manifest.executable
        && !executable.is_absolute()
    {
        return Err(ProviderError::RelativeExecutable {
            path: path.to_path_buf(),
            executable: executable.clone(),
        });
    }
    Ok(())
}

/// Chooses the preferred backend that is actually available on this host.
fn available_backend(manifest: &ProviderManifest) -> Option<LoadedBackend> {
    if let Some(library) = manifest
        .library
        .as_ref()
        .and_then(dylib::LoadedLibrary::load)
    {
        Some(LoadedBackend::Library { library })
    } else if manifest
        .executable
        .as_ref()
        .is_some_and(|path| is_executable_file(path))
    {
        Some(LoadedBackend::Cli)
    } else {
        None
    }
}

/// Reports whether a path names an executable regular file.
fn is_executable_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Provider manifest discovery or validation failure.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Manifest file could not be read.
    #[error("cannot read provider manifest {path:?}: {source}")]
    ReadManifest {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Manifest JSON did not match the strict schema.
    #[error("cannot parse provider manifest {path:?}: {source}")]
    ParseManifest {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// Manifest uses a JSON schema unknown to this release.
    #[error("provider manifest {path:?} uses unsupported schema version {version}")]
    UnsupportedManifestVersion { path: PathBuf, version: u16 },
    /// A library selected for an RPM build is missing or ABI-incompatible.
    #[error("provider {provider:?} cannot load library {library:?} with its required symbols")]
    UnavailableLibrary {
        provider: ProviderId,
        library: PathBuf,
    },
    /// A command selected for an RPM build is missing or not executable.
    #[error("provider {provider:?} has unavailable executable {executable:?}")]
    UnavailableExecutable {
        provider: ProviderId,
        executable: PathBuf,
    },
    /// Manifest declares neither a library nor a command fallback.
    #[error("provider manifest {path:?} declares no backend")]
    MissingBackend { path: PathBuf },
    /// Relative library paths would make privileged loading process-dependent.
    #[error("provider manifest {path:?} has relative library {library:?}")]
    RelativeLibrary { path: PathBuf, library: PathBuf },
    /// A symbol name cannot be passed to the native loader.
    #[error("provider manifest {path:?} has invalid library symbol {symbol:?}")]
    InvalidLibrarySymbol { path: PathBuf, symbol: String },
    /// Relative executable paths would make invocation process-dependent.
    #[error("provider manifest {path:?} has relative executable {executable:?}")]
    RelativeExecutable { path: PathBuf, executable: PathBuf },
    /// More than one available manifest declared the same ID.
    #[error("duplicate provider ID {0:?}")]
    DuplicateProvider(ProviderId),
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
    fn manifest(id: &str, executable: &str) -> ProviderManifest {
        ProviderManifest {
            id: ProviderId::new(id).unwrap_or_else(|| unreachable!()),
            library: None,
            executable: Some(PathBuf::from(executable)),
            capabilities: BTreeSet::from([ProviderCapability::Probe]),
        }
    }

    /// Writes one versioned provider collection fixture.
    fn write_manifests(directory: &Path, manifests: ProviderManifests) -> PathBuf {
        let path = directory.join("providers.json");
        let json = serde_json::to_vec(&manifests)
            .unwrap_or_else(|error| panic!("serialize manifest fixture: {error}"));
        fs::write(&path, json).unwrap_or_else(|error| panic!("write fixture: {error}"));
        path
    }

    /// Wraps provider declarations in the current file schema.
    fn manifests(providers: Vec<ProviderManifest>) -> ProviderManifests {
        ProviderManifests {
            manifest_version: PROVIDER_MANIFEST_VERSION,
            providers,
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

        let registry = ProviderRegistry::load_from(path)
            .unwrap_or_else(|error| panic!("load providers: {error}"));
        ProviderRegistry::validate_dependencies(directory.join("providers.json"))
            .unwrap_or_else(|error| panic!("validate provider dependencies: {error}"));
        assert_eq!(
            registry
                .providers()
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

        let registry = ProviderRegistry::load_from(path)
            .unwrap_or_else(|error| panic!("load providers: {error}"));
        assert_eq!(registry.providers().len(), 0);
        assert!(matches!(
            ProviderRegistry::validate_dependencies(directory.join("providers.json")),
            Err(ProviderError::UnavailableExecutable { .. })
        ));
        fs::remove_dir_all(directory).unwrap_or_else(|error| panic!("remove fixture: {error}"));
    }

    #[test]
    /// Verifies that the fixed version belongs to the manifest schema only.
    fn registry_rejects_unsupported_manifest_schema() {
        let directory = fixture_dir();
        let mut incompatible = manifests(vec![manifest("block", "/bin/true")]);
        incompatible.manifest_version = PROVIDER_MANIFEST_VERSION + 1;
        let path = write_manifests(&directory, incompatible);

        assert!(matches!(
            ProviderRegistry::load_from(path),
            Err(ProviderError::UnsupportedManifestVersion { version, .. })
                if version == PROVIDER_MANIFEST_VERSION + 1
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
            ProviderRegistry::load_from(&path),
            Err(ProviderError::RelativeExecutable { .. })
        ));

        relative.executable = None;
        relative.library = Some(ProviderLibrary {
            path: PathBuf::from("/usr/lib64/libblock.so"),
            required_symbols: BTreeSet::from([String::from("bad\0symbol")]),
        });
        write_manifests(&directory, manifests(vec![relative]));
        assert!(matches!(
            ProviderRegistry::load_from(path),
            Err(ProviderError::InvalidLibrarySymbol { .. })
        ));
        fs::remove_dir_all(directory).unwrap_or_else(|error| panic!("remove fixture: {error}"));
    }
}
