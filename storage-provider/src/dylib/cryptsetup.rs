//! Typed read-only libcryptsetup FFI adapter.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{CryptsetupEntry, CryptsetupKeyslotEntry, CryptsetupKeyslotState, NativeProbeError};

/// Opaque libcryptsetup device context.
enum CryptDeviceRaw {}

type CryptInit = unsafe extern "C" fn(*mut *mut CryptDeviceRaw, *const c_char) -> c_int;
type CryptFree = unsafe extern "C" fn(*mut CryptDeviceRaw);
type CryptLoad = unsafe extern "C" fn(*mut CryptDeviceRaw, *const c_char, *mut c_void) -> c_int;
type CryptGetText = unsafe extern "C" fn(*mut CryptDeviceRaw) -> *const c_char;
type CryptGetU64 = unsafe extern "C" fn(*mut CryptDeviceRaw) -> u64;
type CryptGetInt = unsafe extern "C" fn(*mut CryptDeviceRaw) -> c_int;
type CryptGetMetadataSize = unsafe extern "C" fn(*mut CryptDeviceRaw, *mut u64, *mut u64) -> c_int;
type CryptKeyslotMax = unsafe extern "C" fn(*const c_char) -> c_int;
type CryptKeyslotStatus = unsafe extern "C" fn(*mut CryptDeviceRaw, c_int) -> c_int;

/// Symbols used by the libcryptsetup adapter, borrowed from its library.
struct Api<'library> {
    init: Symbol<'library, CryptInit>,
    free: Symbol<'library, CryptFree>,
    load: Symbol<'library, CryptLoad>,
    get_type: Symbol<'library, CryptGetText>,
    get_uuid: Symbol<'library, CryptGetText>,
    get_cipher: Symbol<'library, CryptGetText>,
    get_cipher_mode: Symbol<'library, CryptGetText>,
    get_data_offset: Symbol<'library, CryptGetU64>,
    get_sector_size: Symbol<'library, CryptGetInt>,
    get_volume_key_size: Symbol<'library, CryptGetInt>,
    get_metadata_size: Symbol<'library, CryptGetMetadataSize>,
    keyslot_max: Symbol<'library, CryptKeyslotMax>,
    keyslot_status: Symbol<'library, CryptKeyslotStatus>,
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
                get_cipher: load_symbol(library, b"crypt_get_cipher\0")?,
                get_cipher_mode: load_symbol(library, b"crypt_get_cipher_mode\0")?,
                get_data_offset: load_symbol(library, b"crypt_get_data_offset\0")?,
                get_sector_size: load_symbol(library, b"crypt_get_sector_size\0")?,
                get_volume_key_size: load_symbol(library, b"crypt_get_volume_key_size\0")?,
                get_metadata_size: load_symbol(library, b"crypt_get_metadata_size\0")?,
                keyslot_max: load_symbol(library, b"crypt_keyslot_max\0")?,
                keyslot_status: load_symbol(library, b"crypt_keyslot_status\0")?,
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
    // SAFETY: loaded metadata remains live for all borrowed getter calls.
    let (cipher, cipher_mode, data_offset_sectors, sector_size, volume_key_size) = unsafe {
        (
            copied_string((api.get_cipher)(context.pointer)),
            copied_string((api.get_cipher_mode)(context.pointer)),
            (api.get_data_offset)(context.pointer),
            positive_u32((api.get_sector_size)(context.pointer)),
            positive_u32((api.get_volume_key_size)(context.pointer)),
        )
    };
    let mut metadata_size = 0;
    let mut keyslots_size = 0;
    // SAFETY: output storage is valid and the loaded context remains live.
    let metadata_result =
        unsafe { (api.get_metadata_size)(context.pointer, &mut metadata_size, &mut keyslots_size) };
    let keyslots = read_keyslots(api, context.pointer, &luks_type);
    Ok(CryptsetupEntry {
        device: device.to_path_buf(),
        luks_type,
        uuid,
        cipher,
        cipher_mode,
        data_offset_sectors,
        sector_size,
        volume_key_size,
        metadata_size: (metadata_result == 0).then_some(metadata_size),
        keyslots_size: (metadata_result == 0).then_some(keyslots_size),
        keyslots,
    })
}

/// Copies all non-inactive keyslot states from one loaded header.
fn read_keyslots(
    api: &Api<'_>,
    device: *mut CryptDeviceRaw,
    luks_type: &str,
) -> Vec<CryptsetupKeyslotEntry> {
    let Ok(native_type) = CString::new(luks_type) else {
        return Vec::new();
    };
    // SAFETY: type is a NUL-terminated libcryptsetup type string.
    let count = unsafe { (api.keyslot_max)(native_type.as_ptr()) };
    if count <= 0 {
        return Vec::new();
    }
    (0..count)
        .filter_map(|index| {
            // SAFETY: context is loaded and index is below keyslot_max.
            let native = unsafe { (api.keyslot_status)(device, index) };
            let state = match native {
                -1 | 0 => return None,
                1 => CryptsetupKeyslotState::Active,
                2 => CryptsetupKeyslotState::ActiveLast,
                3 => CryptsetupKeyslotState::Unbound,
                other => CryptsetupKeyslotState::Other(other),
            };
            Some(CryptsetupKeyslotEntry {
                index: index as u32,
                state,
            })
        })
        .collect()
}

/// Converts a positive native integer into its public unsigned form.
fn positive_u32(value: c_int) -> Option<u32> {
    (value > 0).then_some(value as u32)
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
