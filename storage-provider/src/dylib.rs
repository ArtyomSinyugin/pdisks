#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

//! Isolated dynamic-library loading boundary.

use std::{ffi::c_void, mem::ManuallyDrop, path::PathBuf};

use libloading::{Library, Symbol};

use crate::{
    BlkidEntry, BtrfsCapabilities, BtrfsEntry, BtrfsFilesystemEntry, CryptsetupEntry,
    DevmapperEntry, FdiskTable, FilesystemCapabilities, LibmountEntry, LoopEntry, LvmEntry,
    MdraidEntry, MultipathEntry, NativeProbeError, NvmeEntry, ProviderLibrary, SwapEntry,
    UdevBlockEntry, ZfsEntry,
};

mod blkid;
mod btrfs;
mod btrfs_blockdev;
mod cryptsetup;
mod devmapper;
mod fdisk;
mod filesystem;
mod libmount;
mod loopdev;
mod lvm;
mod mdraid;
mod multipath;
mod nvme;
mod swap;
mod udev;
mod zfs;

/// Loaded native library retained for a provider-specific typed FFI adapter.
pub(super) struct LoadedLibrary {
    /// Process-lifetime handle for a library whose initializers may register
    /// global state that cannot be undone safely by `dlclose`.
    handle: ManuallyDrop<Library>,
    /// Explicit dependency handles retained for multi-library adapters.
    auxiliary: ManuallyDrop<Vec<Library>>,
}

impl std::fmt::Debug for LoadedLibrary {
    /// Avoids exposing platform-specific loader internals in diagnostics.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LoadedLibrary")
    }
}

impl LoadedLibrary {
    /// Loads a native library and resolves every symbol declared by its manifest.
    pub(super) fn load(requirement: &ProviderLibrary) -> Option<Self> {
        // SAFETY: loading a library may run its loader hooks. The manifest path
        // is absolute and package-owned; the handle is retained in `Self`.
        let Ok(library) = (unsafe { Library::new(&requirement.path) }) else {
            return None;
        };
        if !has_symbols(&library, &requirement.required_symbols) {
            return None;
        }
        let mut auxiliary = Vec::with_capacity(requirement.auxiliary.len());
        for dependency in &requirement.auxiliary {
            // SAFETY: auxiliary paths are absolute and package-owned; handles
            // are retained for the process lifetime after successful loading.
            let Ok(handle) = (unsafe { Library::new(&dependency.path) }) else {
                return None;
            };
            if !has_symbols(&handle, &dependency.required_symbols) {
                return None;
            }
            auxiliary.push(handle);
        }
        Some(Self {
            handle: ManuallyDrop::new(library),
            auxiliary: ManuallyDrop::new(auxiliary),
        })
    }

    /// Reads `/proc/self/mountinfo` through libmount.
    pub(super) fn probe_libmount(&self) -> Result<Vec<LibmountEntry>, NativeProbeError> {
        libmount::probe(&self.handle)
    }

    /// Enumerates Linux block endpoints through libudev.
    pub(super) fn probe_udev_blocks(&self) -> Result<Vec<UdevBlockEntry>, NativeProbeError> {
        udev::probe(&self.handle)
    }

    /// Reads partition tables through libfdisk.
    pub(super) fn probe_fdisk_partitions(
        &self,
        devices: &[PathBuf],
    ) -> Result<Vec<FdiskTable>, NativeProbeError> {
        fdisk::probe(&self.handle, devices)
    }

    /// Reads block-content signatures through libblkid.
    pub(super) fn probe_blkid_signatures(&self) -> Result<Vec<BlkidEntry>, NativeProbeError> {
        blkid::probe(&self.handle)
    }

    /// Reads live device-mapper topology through libdevmapper.
    pub(super) fn probe_devmapper(&self) -> Result<Vec<DevmapperEntry>, NativeProbeError> {
        devmapper::probe(&self.handle)
    }

    /// Reads LUKS headers through libcryptsetup.
    pub(super) fn probe_cryptsetup(
        &self,
        devices: &[PathBuf],
    ) -> Result<Vec<CryptsetupEntry>, NativeProbeError> {
        cryptsetup::probe(&self.handle, devices)
    }

    /// Reads loop-device metadata through libblockdev.
    pub(super) fn probe_loop(
        &self,
        devices: &[PathBuf],
    ) -> Result<Vec<LoopEntry>, NativeProbeError> {
        loopdev::probe(&self.handle, devices)
    }

    /// Reads swap activation state through libblockdev.
    pub(super) fn probe_swap(
        &self,
        devices: &[PathBuf],
    ) -> Result<Vec<SwapEntry>, NativeProbeError> {
        swap::probe(&self.handle, devices)
    }

    /// Reads multipath members through libblockdev.
    pub(super) fn probe_multipath(&self) -> Result<MultipathEntry, NativeProbeError> {
        multipath::probe(&self.handle)
    }

    /// Reads MD arrays and member metadata through libblockdev.
    pub(super) fn probe_mdraid(
        &self,
        arrays: &[PathBuf],
        members: &[PathBuf],
    ) -> Result<MdraidEntry, NativeProbeError> {
        mdraid::probe(&self.handle, arrays, members)
    }

    /// Reads LVM topology through libblockdev.
    pub(super) fn probe_lvm(&self) -> Result<LvmEntry, NativeProbeError> {
        lvm::probe(&self.handle)
    }

    /// Reads Btrfs subvolumes below mounted filesystem roots.
    pub(super) fn probe_btrfs(
        &self,
        mountpoints: &[PathBuf],
    ) -> Result<Vec<BtrfsEntry>, NativeProbeError> {
        btrfs::probe(&self.handle, mountpoints)
    }

    /// Reads Btrfs filesystem and member topology through libblockdev.
    pub(super) fn probe_btrfs_filesystems(
        &self,
        devices: &[PathBuf],
    ) -> Result<Vec<BtrfsFilesystemEntry>, NativeProbeError> {
        btrfs_blockdev::probe(&self.handle, devices)
    }

    /// Reports operation groups exposed by libblockdev-btrfs.
    pub(super) fn probe_btrfs_capabilities(&self) -> Result<BtrfsCapabilities, NativeProbeError> {
        btrfs_blockdev::probe_capabilities(&self.handle)
    }

    /// Reads imported ZFS pools and datasets.
    pub(super) fn probe_zfs(&self) -> Result<ZfsEntry, NativeProbeError> {
        zfs::probe(self)
    }

    /// Reads the live NVMe subsystem topology.
    pub(super) fn probe_nvme(&self) -> Result<NvmeEntry, NativeProbeError> {
        nvme::probe(&self.handle)
    }

    /// Reports ordinary-filesystem features and host tool availability.
    pub(super) fn probe_filesystem_capabilities(
        &self,
    ) -> Result<Vec<FilesystemCapabilities>, NativeProbeError> {
        filesystem::probe_capabilities(&self.handle)
    }
}

/// Resolves all declared symbols from one freshly loaded library.
fn has_symbols(library: &Library, symbols: &std::collections::BTreeSet<String>) -> bool {
    symbols.iter().all(|symbol| {
        let mut name = Vec::with_capacity(symbol.len() + 1);
        name.extend_from_slice(symbol.as_bytes());
        name.push(0);
        // SAFETY: manifest validation rejects interior NUL bytes; this only
        // verifies presence and never calls the untyped address.
        unsafe { library.get::<*const c_void>(name.as_slice()) }.is_ok()
    })
}

/// Loads a typed symbol from the main library or an explicit auxiliary.
unsafe fn load_symbol_from_set<'library, T>(
    libraries: &'library LoadedLibrary,
    name: &'static [u8],
) -> Result<Symbol<'library, T>, NativeProbeError> {
    // Prefer explicit auxiliaries so a symbol is borrowed from the library
    // whose ABI declaration the manifest validated.
    for library in libraries.auxiliary.iter() {
        // SAFETY: the caller supplies the exact signature and `Symbol` remains
        // borrowed from a retained library handle.
        if let Ok(symbol) = unsafe { library.get::<T>(name) } {
            return Ok(symbol);
        }
    }
    // SAFETY: same signature and lifetime invariant as auxiliary handles.
    if let Ok(symbol) = unsafe { libraries.handle.get::<T>(name) } {
        return Ok(symbol);
    }
    Err(NativeProbeError::MissingSymbol {
        symbol: symbol_name(name),
    })
}

/// Loads one typed symbol while preserving its borrow from the library.
unsafe fn load_symbol<'library, T>(
    library: &'library Library,
    name: &'static [u8],
) -> Result<Symbol<'library, T>, NativeProbeError> {
    // SAFETY: the caller supplies the exact C signature for `name`; `Symbol`
    // retains the library lifetime and cannot outlive the loaded handle.
    unsafe { library.get::<T>(name) }.map_err(|_| NativeProbeError::MissingSymbol {
        symbol: symbol_name(name),
    })
}

/// Returns a static printable symbol name without its terminating NUL.
fn symbol_name(name: &'static [u8]) -> &'static str {
    let name = name.strip_suffix(&[0]).unwrap_or(name);
    std::str::from_utf8(name).unwrap_or("invalid symbol name")
}

#[cfg(test)]
mod tests {
    use super::LoadedLibrary;

    #[test]
    /// Guards the process-lifetime handle required by libraries with global registries.
    fn loaded_library_handle_is_not_dropped() {
        assert!(!std::mem::needs_drop::<LoadedLibrary>());
    }
}
