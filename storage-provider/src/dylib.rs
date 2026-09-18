#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

//! Isolated dynamic-library loading boundary.

use std::{ffi::c_void, path::PathBuf};

use libloading::{Library, Symbol};

use crate::{
    BlkidEntry, BtrfsEntry, CryptsetupEntry, DevmapperEntry, FdiskTable, LibmountEntry, LoopEntry,
    LvmEntry, MdraidEntry, MultipathEntry, NativeProbeError, NvmeEntry, ProviderLibrary, SwapEntry,
    UdevBlockEntry, ZfsEntry,
};

mod blkid;
mod btrfs;
mod cryptsetup;
mod devmapper;
mod fdisk;
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
    handle: Library,
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

        for symbol in &requirement.required_symbols {
            let mut name = Vec::with_capacity(symbol.len() + 1);
            name.extend_from_slice(symbol.as_bytes());
            name.push(0);
            // SAFETY: validation rejects interior NUL bytes, the appended NUL
            // terminates the name, and the raw address is not called here.
            let resolved: Result<Symbol<'_, *const c_void>, _> =
                unsafe { library.get(name.as_slice()) };
            if resolved.is_err() {
                return None;
            }
        }
        Some(Self { handle: library })
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

    /// Reads imported ZFS pools and datasets.
    pub(super) fn probe_zfs(&self) -> Result<ZfsEntry, NativeProbeError> {
        zfs::probe(&self.handle)
    }

    /// Reads the live NVMe subsystem topology.
    pub(super) fn probe_nvme(&self) -> Result<NvmeEntry, NativeProbeError> {
        nvme::probe(&self.handle)
    }
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
