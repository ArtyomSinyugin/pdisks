//! Typed libblockdev-btrfs FFI adapter.

use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, c_char, c_int, c_void},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{BtrfsCapabilities, BtrfsDeviceEntry, BtrfsFilesystemEntry, NativeProbeError};

const MAX_NATIVE_ENTRIES: usize = 65_536;
const TECH_FILESYSTEM: c_int = 0;
const TECH_MULTI_DEVICE: c_int = 1;
const TECH_SUBVOLUME: c_int = 2;
const TECH_SNAPSHOT: c_int = 3;
const MODE_CREATE: u64 = 1 << 0;
const MODE_DELETE: u64 = 1 << 1;
const MODE_MODIFY: u64 = 1 << 2;
const MODE_QUERY: u64 = 1 << 3;

/// C representation of `BDBtrfsDeviceInfo`.
#[repr(C)]
struct DeviceRaw {
    id: u64,
    path: *mut c_char,
    size: u64,
    used: u64,
}

/// C representation of `BDBtrfsFilesystemInfo`.
#[repr(C)]
struct FilesystemRaw {
    label: *mut c_char,
    uuid: *mut c_char,
    num_devices: u64,
    used: u64,
}

type Init = unsafe extern "C" fn() -> c_int;
type Close = unsafe extern "C" fn();
type IsAvailable = unsafe extern "C" fn(c_int, u64, *mut *mut c_void) -> c_int;
type FilesystemInfo = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> *mut FilesystemRaw;
type FilesystemInfoFree = unsafe extern "C" fn(*mut FilesystemRaw);
type ListDevices = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> *mut *mut DeviceRaw;
type DeviceInfoFree = unsafe extern "C" fn(*mut DeviceRaw);
type GFree = unsafe extern "C" fn(*mut c_void);

/// Symbols used by the standalone libblockdev-btrfs adapter.
struct Api<'library> {
    init: Symbol<'library, Init>,
    close: Symbol<'library, Close>,
    is_available: Symbol<'library, IsAvailable>,
    filesystem_info: Symbol<'library, FilesystemInfo>,
    filesystem_info_free: Symbol<'library, FilesystemInfoFree>,
    list_devices: Symbol<'library, ListDevices>,
    device_info_free: Symbol<'library, DeviceInfoFree>,
    g_free: Symbol<'library, GFree>,
}

impl<'library> Api<'library> {
    /// Resolves the exact libblockdev-btrfs and GLib ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every function type and C structure matches blockdev/btrfs.h;
        // symbols retain the loaded library lifetime.
        unsafe {
            Ok(Self {
                init: load_symbol(library, b"bd_btrfs_init\0")?,
                close: load_symbol(library, b"bd_btrfs_close\0")?,
                is_available: load_symbol(library, b"bd_btrfs_is_tech_avail\0")?,
                filesystem_info: load_symbol(library, b"bd_btrfs_filesystem_info\0")?,
                filesystem_info_free: load_symbol(library, b"bd_btrfs_filesystem_info_free\0")?,
                list_devices: load_symbol(library, b"bd_btrfs_list_devices\0")?,
                device_info_free: load_symbol(library, b"bd_btrfs_device_info_free\0")?,
                g_free: load_symbol(library, b"g_free\0")?,
            })
        }
    }
}

/// Reads and deduplicates filesystems addressed through known Btrfs members.
pub(super) fn probe(
    library: &Library,
    devices: &[PathBuf],
) -> Result<Vec<BtrfsFilesystemEntry>, NativeProbeError> {
    let api = Api::load(library)?;
    let _session = Session::open(&api)?;
    let mut filesystems = BTreeMap::new();
    for device in devices {
        let entry = inspect_filesystem(&api, device)?;
        filesystems.entry(entry.uuid.clone()).or_insert(entry);
    }
    Ok(filesystems.into_values().collect())
}

/// Reports whether every operation mode required by each Btrfs group exists.
pub(super) fn probe_capabilities(library: &Library) -> Result<BtrfsCapabilities, NativeProbeError> {
    let api = Api::load(library)?;
    let _session = Session::open(&api)?;
    Ok(BtrfsCapabilities {
        filesystem: available(
            &api,
            TECH_FILESYSTEM,
            MODE_CREATE | MODE_MODIFY | MODE_QUERY,
        ),
        multi_device: available(
            &api,
            TECH_MULTI_DEVICE,
            MODE_CREATE | MODE_DELETE | MODE_MODIFY | MODE_QUERY,
        ),
        subvolume: available(
            &api,
            TECH_SUBVOLUME,
            MODE_CREATE | MODE_DELETE | MODE_MODIFY | MODE_QUERY,
        ),
        snapshot: available(&api, TECH_SNAPSHOT, MODE_CREATE | MODE_DELETE | MODE_QUERY),
    })
}

/// Queries one Btrfs technology/mode combination.
fn available(api: &Api<'_>, technology: c_int, modes: u64) -> bool {
    // SAFETY: enum and bitmask values match blockdev/btrfs.h; null GError
    // storage is accepted because unavailability is returned as a boolean.
    unsafe { (api.is_available)(technology, modes, std::ptr::null_mut()) != 0 }
}

/// Reads one filesystem and all member devices visible through it.
fn inspect_filesystem(
    api: &Api<'_>,
    device: &Path,
) -> Result<BtrfsFilesystemEntry, NativeProbeError> {
    let path =
        CString::new(device.as_os_str().as_bytes()).map_err(|_| NativeProbeError::InvalidPath {
            path: device.to_path_buf(),
        })?;
    // SAFETY: path is NUL-terminated and null GError storage is accepted.
    let raw = unsafe { (api.filesystem_info)(path.as_ptr(), std::ptr::null_mut()) };
    if raw.is_null() {
        return Err(NativeProbeError::InvalidData {
            operation: "bd_btrfs_filesystem_info",
        });
    }
    let info = FilesystemGuard { pointer: raw, api };
    // SAFETY: all fields belong to the live filesystem-info allocation.
    let (uuid, label, device_count, used) = unsafe {
        (
            copied_string((*info.pointer).uuid),
            copied_nonempty((*info.pointer).label),
            (*info.pointer).num_devices,
            (*info.pointer).used,
        )
    };
    let Some(uuid) = uuid else {
        return Err(NativeProbeError::InvalidData {
            operation: "bd_btrfs_filesystem_info",
        });
    };
    let devices = read_devices(api, &path)?;
    Ok(BtrfsFilesystemEntry {
        seed_device: device.to_path_buf(),
        uuid,
        label,
        device_count,
        used,
        devices,
    })
}

/// Copies the null-terminated member-device array.
fn read_devices(
    api: &Api<'_>,
    device: &CString,
) -> Result<Vec<BtrfsDeviceEntry>, NativeProbeError> {
    // SAFETY: device is NUL-terminated and null GError storage is accepted.
    let pointer = unsafe { (api.list_devices)(device.as_ptr(), std::ptr::null_mut()) };
    if pointer.is_null() {
        return Err(NativeProbeError::InvalidData {
            operation: "bd_btrfs_list_devices",
        });
    }
    let array = ArrayGuard {
        pointer: pointer.cast(),
        api,
    };
    let mut devices = Vec::new();
    for index in 0..MAX_NATIVE_ENTRIES {
        // SAFETY: the provider returns a null-terminated live pointer array.
        let raw = unsafe { *pointer.add(index) };
        if raw.is_null() {
            drop(array);
            return Ok(devices);
        }
        let item = DeviceGuard { pointer: raw, api };
        // SAFETY: fields belong to the live device-info allocation.
        let path = unsafe { copied_string((*item.pointer).path) };
        if let Some(path) = path {
            // SAFETY: scalar fields belong to the same allocation.
            devices.push(unsafe {
                BtrfsDeviceEntry {
                    id: (*item.pointer).id,
                    path: PathBuf::from(path),
                    size: (*item.pointer).size,
                    used: (*item.pointer).used,
                }
            });
        }
    }
    drop(array);
    Err(NativeProbeError::InvalidData {
        operation: "bd_btrfs_list_devices",
    })
}

/// Copies a nullable native string.
unsafe fn copied_string(pointer: *const c_char) -> Option<String> {
    (!pointer.is_null()).then(|| {
        // SAFETY: caller guarantees a borrowed NUL-terminated native string.
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    })
}

/// Copies a nullable non-empty native string.
unsafe fn copied_nonempty(pointer: *const c_char) -> Option<String> {
    // SAFETY: forwarded from the caller under the same string invariant.
    unsafe { copied_string(pointer) }.filter(|value| !value.is_empty())
}

/// Owns one initialized plugin session.
struct Session<'api, 'library> {
    api: &'api Api<'library>,
}

impl<'api, 'library> Session<'api, 'library> {
    /// Initializes a standalone libblockdev-btrfs plugin session.
    fn open(api: &'api Api<'library>) -> Result<Self, NativeProbeError> {
        // SAFETY: standalone plugin initialization takes no arguments.
        if unsafe { (api.init)() } == 0 {
            return Err(NativeProbeError::CallFailed {
                operation: "bd_btrfs_init",
                code: 0,
            });
        }
        Ok(Self { api })
    }
}

impl Drop for Session<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one successful `bd_btrfs_init` call.
        unsafe { (self.api.close)() };
    }
}

/// Owns one filesystem-info result.
struct FilesystemGuard<'api, 'library> {
    pointer: *mut FilesystemRaw,
    api: &'api Api<'library>,
}

impl Drop for FilesystemGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one filesystem-info allocation.
        unsafe { (self.api.filesystem_info_free)(self.pointer) };
    }
}

/// Owns one member-device result.
struct DeviceGuard<'api, 'library> {
    pointer: *mut DeviceRaw,
    api: &'api Api<'library>,
}

impl Drop for DeviceGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one device-info allocation.
        unsafe { (self.api.device_info_free)(self.pointer) };
    }
}

/// Owns the GLib-allocated outer device pointer array.
struct ArrayGuard<'api, 'library> {
    pointer: *mut c_void,
    api: &'api Api<'library>,
}

impl Drop for ArrayGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: outer array is released once after all elements are freed.
        unsafe { (self.api.g_free)(self.pointer) };
    }
}
