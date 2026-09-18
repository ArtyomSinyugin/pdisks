//! Typed read-only libblockdev multipath-plugin FFI adapter.

use std::{
    ffi::{CStr, OsString, c_char, c_int, c_void},
    os::unix::ffi::OsStringExt,
    path::PathBuf,
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{MultipathEntry, NativeProbeError};

const MAX_NATIVE_ENTRIES: usize = 65_536;

type Init = unsafe extern "C" fn() -> c_int;
type Close = unsafe extern "C" fn();
type GetMembers = unsafe extern "C" fn(*mut *mut c_void) -> *mut *mut c_char;
type StrFreev = unsafe extern "C" fn(*mut *mut c_char);

/// Symbols used by the libblockdev multipath adapter.
struct Api<'library> {
    init: Symbol<'library, Init>,
    close: Symbol<'library, Close>,
    get_members: Symbol<'library, GetMembers>,
    str_freev: Symbol<'library, StrFreev>,
}

impl<'library> Api<'library> {
    /// Resolves the exact multipath-plugin and GLib ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: function types match mpath.h and glib/gstrfuncs.h. GLib is a
        // dependency of the plugin, so its allocator symbol shares the handle.
        unsafe {
            Ok(Self {
                init: load_symbol(library, b"bd_mpath_init\0")?,
                close: load_symbol(library, b"bd_mpath_close\0")?,
                get_members: load_symbol(library, b"bd_mpath_get_mpath_members\0")?,
                str_freev: load_symbol(library, b"g_strfreev\0")?,
            })
        }
    }
}

/// Reads paths currently recognized as multipath members.
pub(super) fn probe(library: &Library) -> Result<MultipathEntry, NativeProbeError> {
    let api = Api::load(library)?;
    // SAFETY: standalone plugin initialization takes no arguments.
    if unsafe { (api.init)() } == 0 {
        return Err(NativeProbeError::CallFailed {
            operation: "bd_mpath_init",
            code: 0,
        });
    }
    let _session = Session { api: &api };
    // SAFETY: null GError storage is accepted by GLib; a non-null result is a
    // null-terminated string vector owned by the caller.
    let pointer = unsafe { (api.get_members)(std::ptr::null_mut()) };
    if pointer.is_null() {
        return Ok(MultipathEntry::default());
    }
    let members = StringVector { pointer, api: &api };
    let mut paths = Vec::new();
    for index in 0..MAX_NATIVE_ENTRIES {
        // SAFETY: the vector is null-terminated and remains live.
        let value = unsafe { *members.pointer.add(index) };
        if value.is_null() {
            return Ok(MultipathEntry { members: paths });
        }
        // SAFETY: each non-null vector entry is a NUL-terminated GLib string.
        paths.push(PathBuf::from(OsString::from_vec(
            unsafe { CStr::from_ptr(value) }.to_bytes().to_vec(),
        )));
    }
    Err(NativeProbeError::InvalidData {
        operation: "bd_mpath_get_mpath_members",
    })
}

/// Closes one initialized standalone plugin session.
struct Session<'api, 'library> {
    api: &'api Api<'library>,
}

impl Drop for Session<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one successful `bd_mpath_init` call.
        unsafe { (self.api.close)() };
    }
}

/// Owns one GLib string vector.
struct StringVector<'api, 'library> {
    pointer: *mut *mut c_char,
    api: &'api Api<'library>,
}

impl Drop for StringVector<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: vector and its strings were allocated by GLib and are freed once.
        unsafe { (self.api.str_freev)(self.pointer) };
    }
}
