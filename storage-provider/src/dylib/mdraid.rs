//! Typed read-only libblockdev mdraid-plugin FFI adapter.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{MdraidArrayEntry, MdraidEntry, MdraidMemberEntry, NativeProbeError};

/// C layout of `BDMDExamineData`.
#[repr(C)]
struct ExamineRaw {
    device: *mut c_char,
    level: *mut c_char,
    num_devices: u64,
    name: *mut c_char,
    size: u64,
    uuid: *mut c_char,
    update_time: u64,
    dev_uuid: *mut c_char,
    events: u64,
    metadata: *mut c_char,
    chunk_size: u64,
}

/// C layout of `BDMDDetailData`.
#[repr(C)]
struct DetailRaw {
    device: *mut c_char,
    metadata: *mut c_char,
    creation_time: *mut c_char,
    level: *mut c_char,
    name: *mut c_char,
    array_size: u64,
    use_dev_size: u64,
    raid_devices: u64,
    total_devices: u64,
    active_devices: u64,
    working_devices: u64,
    failed_devices: u64,
    spare_devices: u64,
    clean: c_int,
    uuid: *mut c_char,
    container: *mut c_char,
}

type Init = unsafe extern "C" fn() -> c_int;
type Close = unsafe extern "C" fn();
type Detail = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> *mut DetailRaw;
type DetailFree = unsafe extern "C" fn(*mut DetailRaw);
type Examine = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> *mut ExamineRaw;
type ExamineFree = unsafe extern "C" fn(*mut ExamineRaw);

/// Symbols used by the libblockdev mdraid adapter.
struct Api<'library> {
    init: Symbol<'library, Init>,
    close: Symbol<'library, Close>,
    detail: Symbol<'library, Detail>,
    detail_free: Symbol<'library, DetailFree>,
    examine: Symbol<'library, Examine>,
    examine_free: Symbol<'library, ExamineFree>,
}

impl<'library> Api<'library> {
    /// Resolves the exact mdraid-plugin ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every Rust function and structure type matches mdraid.h.
        unsafe {
            Ok(Self {
                init: load_symbol(library, b"bd_md_init\0")?,
                close: load_symbol(library, b"bd_md_close\0")?,
                detail: load_symbol(library, b"bd_md_detail\0")?,
                detail_free: load_symbol(library, b"bd_md_detail_data_free\0")?,
                examine: load_symbol(library, b"bd_md_examine\0")?,
                examine_free: load_symbol(library, b"bd_md_examine_data_free\0")?,
            })
        }
    }
}

/// Reads active arrays and known member superblocks.
pub(super) fn probe(
    library: &Library,
    arrays: &[PathBuf],
    members: &[PathBuf],
) -> Result<MdraidEntry, NativeProbeError> {
    let api = Api::load(library)?;
    // SAFETY: standalone plugin initialization takes no arguments.
    if unsafe { (api.init)() } == 0 {
        return Err(call_failed("bd_md_init"));
    }
    let _session = Session { api: &api };
    let arrays = arrays
        .iter()
        .filter_map(|device| inspect_array(&api, device).transpose())
        .collect::<Result<Vec<_>, _>>()?;
    let members = members
        .iter()
        .filter_map(|device| inspect_member(&api, device).transpose())
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MdraidEntry { arrays, members })
}

/// Copies one active MD array, skipping unused kernel MD slots.
fn inspect_array(
    api: &Api<'_>,
    device: &Path,
) -> Result<Option<MdraidArrayEntry>, NativeProbeError> {
    let path = native_path(device)?;
    // SAFETY: path is NUL-terminated; null GError storage is accepted by GLib.
    let pointer = unsafe { (api.detail)(path.as_ptr(), std::ptr::null_mut()) };
    if pointer.is_null() {
        return Ok(None);
    }
    let detail = DetailGuard { pointer, api };
    // SAFETY: strings and scalar fields belong to the live detail structure.
    let level = unsafe { copied_string((*detail.pointer).level) };
    let Some(level) = level else {
        return Ok(None);
    };
    // SAFETY: all fields are copied before the guard releases the structure.
    Ok(Some(unsafe {
        MdraidArrayEntry {
            device: device.to_path_buf(),
            level,
            metadata: copied_string((*detail.pointer).metadata),
            uuid: copied_string((*detail.pointer).uuid),
            size: (*detail.pointer).array_size,
            raid_devices: (*detail.pointer).raid_devices,
        }
    }))
}

/// Copies one MD member superblock.
fn inspect_member(
    api: &Api<'_>,
    device: &Path,
) -> Result<Option<MdraidMemberEntry>, NativeProbeError> {
    let path = native_path(device)?;
    // SAFETY: path is NUL-terminated; null GError storage is accepted by GLib.
    let pointer = unsafe { (api.examine)(path.as_ptr(), std::ptr::null_mut()) };
    if pointer.is_null() {
        return Ok(None);
    }
    let examine = ExamineGuard { pointer, api };
    // SAFETY: all fields are copied before the guard releases the structure.
    Ok(Some(unsafe {
        MdraidMemberEntry {
            device: device.to_path_buf(),
            level: copied_string((*examine.pointer).level),
            array_uuid: copied_string((*examine.pointer).uuid),
            device_uuid: copied_string((*examine.pointer).dev_uuid),
            metadata: copied_string((*examine.pointer).metadata),
            chunk_size: (*examine.pointer).chunk_size,
        }
    }))
}

/// Converts a Unix path into a native C string.
fn native_path(path: &Path) -> Result<CString, NativeProbeError> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| NativeProbeError::InvalidPath {
        path: path.to_path_buf(),
    })
}

/// Closes one initialized standalone plugin session.
struct Session<'api, 'library> {
    api: &'api Api<'library>,
}

impl Drop for Session<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one successful `bd_md_init` call.
        unsafe { (self.api.close)() };
    }
}

/// Owns one MD detail allocation.
struct DetailGuard<'api, 'library> {
    pointer: *mut DetailRaw,
    api: &'api Api<'library>,
}

impl Drop for DetailGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances the allocation returned by `bd_md_detail`.
        unsafe { (self.api.detail_free)(self.pointer) };
    }
}

/// Owns one MD examine allocation.
struct ExamineGuard<'api, 'library> {
    pointer: *mut ExamineRaw,
    api: &'api Api<'library>,
}

impl Drop for ExamineGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances the allocation returned by `bd_md_examine`.
        unsafe { (self.api.examine_free)(self.pointer) };
    }
}

/// Copies an optional native string as lossy UTF-8.
unsafe fn copied_string(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: caller guarantees a valid NUL-terminated borrowed C string.
    Some(
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Creates the shared error for a GLib boolean failure.
fn call_failed(operation: &'static str) -> NativeProbeError {
    NativeProbeError::CallFailed { operation, code: 0 }
}
