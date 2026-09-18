//! Typed libblkid FFI adapter.

use std::{
    ffi::{CStr, OsString, c_char, c_int},
    os::unix::ffi::OsStringExt,
    path::PathBuf,
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{BlkidEntry, NativeProbeError};

/// Opaque libblkid cache.
enum CacheRaw {}
/// Opaque libblkid device iterator.
enum DevIterRaw {}
/// Opaque libblkid device.
enum DevRaw {}
/// Opaque libblkid tag iterator.
enum TagIterRaw {}

type GetCache = unsafe extern "C" fn(*mut *mut CacheRaw, *const c_char) -> c_int;
type PutCache = unsafe extern "C" fn(*mut CacheRaw);
type ProbeAll = unsafe extern "C" fn(*mut CacheRaw) -> c_int;
type IterBegin = unsafe extern "C" fn(*mut CacheRaw) -> *mut DevIterRaw;
type IterEnd = unsafe extern "C" fn(*mut DevIterRaw);
type DevNext = unsafe extern "C" fn(*mut DevIterRaw, *mut *mut DevRaw) -> c_int;
type DevName = unsafe extern "C" fn(*mut DevRaw) -> *const c_char;
type TagBegin = unsafe extern "C" fn(*mut DevRaw) -> *mut TagIterRaw;
type TagNext =
    unsafe extern "C" fn(*mut TagIterRaw, *mut *const c_char, *mut *const c_char) -> c_int;
type TagEnd = unsafe extern "C" fn(*mut TagIterRaw);

/// Symbols used by the libblkid adapter, borrowed from its loaded library.
struct Api<'library> {
    get_cache: Symbol<'library, GetCache>,
    put_cache: Symbol<'library, PutCache>,
    probe_all: Symbol<'library, ProbeAll>,
    iter_begin: Symbol<'library, IterBegin>,
    iter_end: Symbol<'library, IterEnd>,
    dev_next: Symbol<'library, DevNext>,
    dev_name: Symbol<'library, DevName>,
    tag_begin: Symbol<'library, TagBegin>,
    tag_next: Symbol<'library, TagNext>,
    tag_end: Symbol<'library, TagEnd>,
}

impl<'library> Api<'library> {
    /// Resolves the exact libblkid ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every Rust function type below matches blkid.h. Symbols
        // retain `library`'s lifetime.
        unsafe {
            Ok(Self {
                get_cache: load_symbol(library, b"blkid_get_cache\0")?,
                put_cache: load_symbol(library, b"blkid_put_cache\0")?,
                probe_all: load_symbol(library, b"blkid_probe_all\0")?,
                iter_begin: load_symbol(library, b"blkid_dev_iterate_begin\0")?,
                iter_end: load_symbol(library, b"blkid_dev_iterate_end\0")?,
                dev_next: load_symbol(library, b"blkid_dev_next\0")?,
                dev_name: load_symbol(library, b"blkid_dev_devname\0")?,
                tag_begin: load_symbol(library, b"blkid_tag_iterate_begin\0")?,
                tag_next: load_symbol(library, b"blkid_tag_next\0")?,
                tag_end: load_symbol(library, b"blkid_tag_iterate_end\0")?,
            })
        }
    }
}

/// Enumerates block-content signatures through a borrowed libblkid handle.
pub(super) fn probe(library: &Library) -> Result<Vec<BlkidEntry>, NativeProbeError> {
    let api = Api::load(library)?;
    let mut pointer = std::ptr::null_mut();
    // SAFETY: output pointer is valid and null selects the default cache file.
    check_zero("blkid_get_cache", unsafe {
        (api.get_cache)(&mut pointer, std::ptr::null())
    })?;
    if pointer.is_null() {
        return Err(NativeProbeError::AllocationFailed {
            object: "libblkid cache",
        });
    }
    let cache = Cache { pointer, api: &api };
    // SAFETY: cache is live.
    check_zero("blkid_probe_all", unsafe { (api.probe_all)(cache.pointer) })?;

    // SAFETY: cache is live and owns the iterator inputs.
    let pointer = unsafe { (api.iter_begin)(cache.pointer) };
    if pointer.is_null() {
        return Err(NativeProbeError::AllocationFailed {
            object: "libblkid device iterator",
        });
    }
    let iterator = DevIter { pointer, api: &api };
    let mut entries = Vec::new();
    loop {
        let mut device = std::ptr::null_mut();
        // SAFETY: iterator and output pointer are live.
        if unsafe { (api.dev_next)(iterator.pointer, &mut device) } != 0 {
            break;
        }
        if !device.is_null()
            && let Some(entry) = read_device(&api, device)
        {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Copies one device and all relevant tags from the live cache.
fn read_device(api: &Api<'_>, device: *mut DevRaw) -> Option<BlkidEntry> {
    // SAFETY: device is borrowed from the live cache and its name is copied.
    let name = unsafe { copied_bytes((api.dev_name)(device)) }?;
    let mut entry = BlkidEntry {
        device: PathBuf::from(OsString::from_vec(name)),
        signature_type: None,
        uuid: None,
        label: None,
        partition_uuid: None,
    };
    // SAFETY: device is live; a non-null result is an owned iterator.
    let pointer = unsafe { (api.tag_begin)(device) };
    if pointer.is_null() {
        return Some(entry);
    }
    let tags = TagIter { pointer, api };
    loop {
        let mut kind = std::ptr::null();
        let mut value = std::ptr::null();
        // SAFETY: iterator and output pointers are live.
        if unsafe { (api.tag_next)(tags.pointer, &mut kind, &mut value) } != 0 {
            break;
        }
        // SAFETY: tag strings are borrowed from the live cache and copied.
        let (Some(kind), Some(value)) = (unsafe { copied_string(kind) }, unsafe {
            copied_string(value)
        }) else {
            continue;
        };
        match kind.as_str() {
            "TYPE" => entry.signature_type = Some(value),
            "UUID" => entry.uuid = Some(value),
            "LABEL" => entry.label = Some(value),
            "PARTUUID" => entry.partition_uuid = Some(value),
            _ => {}
        }
    }
    Some(entry)
}

/// Owns one libblkid cache.
struct Cache<'api, 'library> {
    pointer: *mut CacheRaw,
    api: &'api Api<'library>,
}

impl Drop for Cache<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: this is the single matching release for the owned cache.
        unsafe { (self.api.put_cache)(self.pointer) };
    }
}

/// Owns one libblkid device iterator.
struct DevIter<'api, 'library> {
    pointer: *mut DevIterRaw,
    api: &'api Api<'library>,
}

impl Drop for DevIter<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: this is the single matching release for the owned iterator.
        unsafe { (self.api.iter_end)(self.pointer) };
    }
}

/// Owns one libblkid tag iterator.
struct TagIter<'api, 'library> {
    pointer: *mut TagIterRaw,
    api: &'api Api<'library>,
}

impl Drop for TagIter<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: this is the single matching release for the owned iterator.
        unsafe { (self.api.tag_end)(self.pointer) };
    }
}

/// Converts libblkid's zero-success result into the shared error.
fn check_zero(operation: &'static str, code: c_int) -> Result<(), NativeProbeError> {
    if code == 0 {
        Ok(())
    } else {
        Err(NativeProbeError::CallFailed { operation, code })
    }
}

/// Copies an optional native string as raw Unix bytes.
unsafe fn copied_bytes(pointer: *const c_char) -> Option<Vec<u8>> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: caller guarantees a valid NUL-terminated borrowed C string.
    Some(unsafe { CStr::from_ptr(pointer) }.to_bytes().to_vec())
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
