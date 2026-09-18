//! Typed read-only libblockdev swap-plugin FFI adapter.

use std::{
    ffi::{CString, c_char, c_int, c_void},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{NativeProbeError, SwapEntry};

type Init = unsafe extern "C" fn() -> c_int;
type Close = unsafe extern "C" fn();
type Status = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> c_int;

/// Symbols used by the libblockdev swap adapter.
struct Api<'library> {
    init: Symbol<'library, Init>,
    close: Symbol<'library, Close>,
    status: Symbol<'library, Status>,
}

impl<'library> Api<'library> {
    /// Resolves the exact swap-plugin ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every Rust function type below matches swap.h.
        unsafe {
            Ok(Self {
                init: load_symbol(library, b"bd_swap_init\0")?,
                close: load_symbol(library, b"bd_swap_close\0")?,
                status: load_symbol(library, b"bd_swap_swapstatus\0")?,
            })
        }
    }
}

/// Reads activation state for known swap signatures.
pub(super) fn probe(
    library: &Library,
    devices: &[PathBuf],
) -> Result<Vec<SwapEntry>, NativeProbeError> {
    let api = Api::load(library)?;
    // SAFETY: standalone plugin initialization takes no arguments.
    if unsafe { (api.init)() } == 0 {
        return Err(NativeProbeError::CallFailed {
            operation: "bd_swap_init",
            code: 0,
        });
    }
    let _session = Session { api: &api };
    devices.iter().map(|device| inspect(&api, device)).collect()
}

/// Reads one swap activation flag.
fn inspect(api: &Api<'_>, device: &Path) -> Result<SwapEntry, NativeProbeError> {
    let path =
        CString::new(device.as_os_str().as_bytes()).map_err(|_| NativeProbeError::InvalidPath {
            path: device.to_path_buf(),
        })?;
    // SAFETY: path is NUL-terminated; null GError storage is accepted by GLib.
    let active = unsafe { (api.status)(path.as_ptr(), std::ptr::null_mut()) } != 0;
    Ok(SwapEntry {
        device: device.to_path_buf(),
        active,
    })
}

/// Closes one initialized standalone plugin session.
struct Session<'api, 'library> {
    api: &'api Api<'library>,
}

impl Drop for Session<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one successful `bd_swap_init` call.
        unsafe { (self.api.close)() };
    }
}
