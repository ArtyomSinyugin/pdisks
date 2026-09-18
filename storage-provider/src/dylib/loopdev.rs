//! Typed read-only libblockdev loop-plugin FFI adapter.

use std::{
    ffi::{CStr, CString, OsString, c_char, c_int, c_void},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{LoopEntry, NativeProbeError};

/// C layout of `BDLoopInfo`.
#[repr(C)]
struct LoopInfoRaw {
    backing_file: *mut c_char,
    offset: u64,
    autoclear: c_int,
    direct_io: c_int,
    part_scan: c_int,
    read_only: c_int,
}

type Init = unsafe extern "C" fn() -> c_int;
type Close = unsafe extern "C" fn();
type Info = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> *mut LoopInfoRaw;
type InfoFree = unsafe extern "C" fn(*mut LoopInfoRaw);

/// Symbols used by the libblockdev loop adapter.
struct Api<'library> {
    init: Symbol<'library, Init>,
    close: Symbol<'library, Close>,
    info: Symbol<'library, Info>,
    info_free: Symbol<'library, InfoFree>,
}

impl<'library> Api<'library> {
    /// Resolves the exact loop-plugin ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every Rust function and structure type matches loop.h.
        unsafe {
            Ok(Self {
                init: load_symbol(library, b"bd_loop_init\0")?,
                close: load_symbol(library, b"bd_loop_close\0")?,
                info: load_symbol(library, b"bd_loop_info\0")?,
                info_free: load_symbol(library, b"bd_loop_info_free\0")?,
            })
        }
    }
}

/// Reads metadata for known loop block devices.
pub(super) fn probe(
    library: &Library,
    devices: &[PathBuf],
) -> Result<Vec<LoopEntry>, NativeProbeError> {
    let api = Api::load(library)?;
    // SAFETY: standalone plugin initialization takes no arguments.
    if unsafe { (api.init)() } == 0 {
        return Err(call_failed("bd_loop_init"));
    }
    let _session = Session { api: &api };
    devices
        .iter()
        .filter_map(|device| match inspect(&api, device) {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

/// Reads one loop device, skipping unused kernel loop slots.
fn inspect(api: &Api<'_>, device: &Path) -> Result<Option<LoopEntry>, NativeProbeError> {
    let path =
        CString::new(device.as_os_str().as_bytes()).map_err(|_| NativeProbeError::InvalidPath {
            path: device.to_path_buf(),
        })?;
    // SAFETY: path is NUL-terminated; null GError storage is accepted by GLib.
    let pointer = unsafe { (api.info)(path.as_ptr(), std::ptr::null_mut()) };
    if pointer.is_null() {
        return Ok(None);
    }
    let info = InfoGuard { pointer, api };
    // SAFETY: info is live and owns a NUL-terminated backing-file string.
    let backing_file = unsafe { copied_path((*info.pointer).backing_file) };
    let Some(backing_file) = backing_file else {
        return Ok(None);
    };
    // SAFETY: all scalar fields belong to the live C structure.
    Ok(Some(unsafe {
        LoopEntry {
            device: device.to_path_buf(),
            backing_file,
            offset: (*info.pointer).offset,
            autoclear: (*info.pointer).autoclear != 0,
            direct_io: (*info.pointer).direct_io != 0,
            part_scan: (*info.pointer).part_scan != 0,
            read_only: (*info.pointer).read_only != 0,
        }
    }))
}

/// Closes one initialized standalone plugin session.
struct Session<'api, 'library> {
    api: &'api Api<'library>,
}

impl Drop for Session<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one successful `bd_loop_init` call.
        unsafe { (self.api.close)() };
    }
}

/// Owns one `BDLoopInfo` allocation.
struct InfoGuard<'api, 'library> {
    pointer: *mut LoopInfoRaw,
    api: &'api Api<'library>,
}

impl Drop for InfoGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances the allocation returned by `bd_loop_info`.
        unsafe { (self.api.info_free)(self.pointer) };
    }
}

/// Copies an optional native pathname.
unsafe fn copied_path(pointer: *const c_char) -> Option<PathBuf> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: caller guarantees a valid NUL-terminated borrowed C string.
    Some(PathBuf::from(OsString::from_vec(
        unsafe { CStr::from_ptr(pointer) }.to_bytes().to_vec(),
    )))
}

/// Creates the shared error for a GLib boolean failure.
fn call_failed(operation: &'static str) -> NativeProbeError {
    NativeProbeError::CallFailed { operation, code: 0 }
}
