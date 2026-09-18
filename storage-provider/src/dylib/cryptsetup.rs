//! Typed read-only libcryptsetup FFI adapter.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{CryptsetupEntry, NativeProbeError};

/// Opaque libcryptsetup device context.
enum CryptDeviceRaw {}

type CryptInit = unsafe extern "C" fn(*mut *mut CryptDeviceRaw, *const c_char) -> c_int;
type CryptFree = unsafe extern "C" fn(*mut CryptDeviceRaw);
type CryptLoad = unsafe extern "C" fn(*mut CryptDeviceRaw, *const c_char, *mut c_void) -> c_int;
type CryptGetText = unsafe extern "C" fn(*mut CryptDeviceRaw) -> *const c_char;

/// Symbols used by the libcryptsetup adapter, borrowed from its library.
struct Api<'library> {
    init: Symbol<'library, CryptInit>,
    free: Symbol<'library, CryptFree>,
    load: Symbol<'library, CryptLoad>,
    get_type: Symbol<'library, CryptGetText>,
    get_uuid: Symbol<'library, CryptGetText>,
}

impl<'library> Api<'library> {
    /// Resolves the exact libcryptsetup ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every Rust function type below matches libcryptsetup.h.
        // Symbols retain `library`'s lifetime.
        unsafe {
            Ok(Self {
                init: load_symbol(library, b"crypt_init\0")?,
                free: load_symbol(library, b"crypt_free\0")?,
                load: load_symbol(library, b"crypt_load\0")?,
                get_type: load_symbol(library, b"crypt_get_type\0")?,
                get_uuid: load_symbol(library, b"crypt_get_uuid\0")?,
            })
        }
    }
}

/// Reads LUKS headers from the supplied block devices.
pub(super) fn probe(
    library: &Library,
    devices: &[PathBuf],
) -> Result<Vec<CryptsetupEntry>, NativeProbeError> {
    let api = Api::load(library)?;
    devices
        .iter()
        .map(|device| inspect_device(&api, device))
        .collect()
}

/// Loads and copies the non-secret metadata from one LUKS header.
fn inspect_device(api: &Api<'_>, device: &Path) -> Result<CryptsetupEntry, NativeProbeError> {
    let path =
        CString::new(device.as_os_str().as_bytes()).map_err(|_| NativeProbeError::InvalidPath {
            path: device.to_path_buf(),
        })?;
    let mut pointer = std::ptr::null_mut();
    // SAFETY: output pointer and NUL-terminated device path are live.
    let result = unsafe { (api.init)(&mut pointer, path.as_ptr()) };
    if result < 0 {
        return Err(NativeProbeError::CallFailed {
            operation: "crypt_init",
            code: result,
        });
    }
    if pointer.is_null() {
        return Err(NativeProbeError::AllocationFailed {
            object: "libcryptsetup context",
        });
    }
    let context = Context { pointer, api };
    // SAFETY: context is live; null type requests automatic LUKS detection and
    // null params selects default read-only metadata loading.
    let result = unsafe { (api.load)(context.pointer, std::ptr::null(), std::ptr::null_mut()) };
    if result < 0 {
        return Err(NativeProbeError::CallFailed {
            operation: "crypt_load",
            code: result,
        });
    }
    // SAFETY: strings are borrowed from the live context and copied.
    let (luks_type, uuid) = unsafe {
        (
            copied_string((api.get_type)(context.pointer)),
            copied_string((api.get_uuid)(context.pointer)),
        )
    };
    let Some(luks_type) = luks_type else {
        return Err(NativeProbeError::InvalidData {
            operation: "crypt_get_type",
        });
    };
    Ok(CryptsetupEntry {
        device: device.to_path_buf(),
        luks_type,
        uuid,
    })
}

/// Owns one libcryptsetup device context.
struct Context<'api, 'library> {
    pointer: *mut CryptDeviceRaw,
    api: &'api Api<'library>,
}

impl Drop for Context<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: this is the single matching release for the owned context.
        unsafe { (self.api.free)(self.pointer) };
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
