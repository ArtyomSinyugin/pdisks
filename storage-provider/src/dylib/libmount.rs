//! Typed libmount FFI adapter.

use std::{
    ffi::{CStr, OsString, c_char, c_int},
    os::unix::ffi::OsStringExt,
    path::PathBuf,
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{LibmountEntry, NativeProbeError};

/// Opaque libmount table handle.
enum TableRaw {}
/// Opaque libmount iterator handle.
enum IterRaw {}
/// Opaque libmount filesystem entry handle.
enum FsRaw {}

type NewTable = unsafe extern "C" fn(*const c_char) -> *mut TableRaw;
type FreeTable = unsafe extern "C" fn(*mut TableRaw);
type NewIter = unsafe extern "C" fn(c_int) -> *mut IterRaw;
type FreeIter = unsafe extern "C" fn(*mut IterRaw);
type NextFs = unsafe extern "C" fn(*mut TableRaw, *mut IterRaw, *mut *mut FsRaw) -> c_int;
type GetField = unsafe extern "C" fn(*mut FsRaw) -> *const c_char;

/// Symbols used by the libmount adapter, borrowed from its loaded library.
struct Api<'library> {
    new_table: Symbol<'library, NewTable>,
    free_table: Symbol<'library, FreeTable>,
    new_iter: Symbol<'library, NewIter>,
    free_iter: Symbol<'library, FreeIter>,
    next_fs: Symbol<'library, NextFs>,
    get_source: Symbol<'library, GetField>,
    get_target: Symbol<'library, GetField>,
    get_fstype: Symbol<'library, GetField>,
    get_options: Symbol<'library, GetField>,
}

impl<'library> Api<'library> {
    /// Resolves the exact libmount ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every Rust function type below matches the corresponding
        // declaration in libmount.h. Symbols retain `library`'s lifetime.
        unsafe {
            Ok(Self {
                new_table: load_symbol(library, b"mnt_new_table_from_file\0")?,
                free_table: load_symbol(library, b"mnt_free_table\0")?,
                new_iter: load_symbol(library, b"mnt_new_iter\0")?,
                free_iter: load_symbol(library, b"mnt_free_iter\0")?,
                next_fs: load_symbol(library, b"mnt_table_next_fs\0")?,
                get_source: load_symbol(library, b"mnt_fs_get_source\0")?,
                get_target: load_symbol(library, b"mnt_fs_get_target\0")?,
                get_fstype: load_symbol(library, b"mnt_fs_get_fstype\0")?,
                get_options: load_symbol(library, b"mnt_fs_get_options\0")?,
            })
        }
    }
}

/// Reads the process mount table through a borrowed libmount handle.
pub(super) fn probe(library: &Library) -> Result<Vec<LibmountEntry>, NativeProbeError> {
    let api = Api::load(library)?;
    // SAFETY: the static path is NUL-terminated; the returned table is either
    // null or an owned libmount reference released by `Table`.
    let pointer = unsafe { (api.new_table)(c"/proc/self/mountinfo".as_ptr()) };
    if pointer.is_null() {
        return Err(NativeProbeError::AllocationFailed {
            object: "libmount table",
        });
    }
    let table = Table { pointer, api: &api };

    // SAFETY: zero is libmount's forward direction; the returned iterator is
    // either null or an owned reference released by `Iter`.
    let pointer = unsafe { (api.new_iter)(0) };
    if pointer.is_null() {
        return Err(NativeProbeError::AllocationFailed {
            object: "libmount iterator",
        });
    }
    let iter = Iter { pointer, api: &api };

    let mut entries = Vec::new();
    loop {
        let mut filesystem = std::ptr::null_mut();
        // SAFETY: table and iterator are live and the output pointer is valid.
        let result = unsafe { (api.next_fs)(table.pointer, iter.pointer, &mut filesystem) };
        if result > 0 {
            break;
        }
        if result < 0 || filesystem.is_null() {
            return Err(NativeProbeError::CallFailed {
                operation: "mnt_table_next_fs",
                code: result,
            });
        }

        // SAFETY: getters return strings borrowed from the live filesystem;
        // every value is copied before the next iteration.
        let (source, target, filesystem_type, options) = unsafe {
            (
                copied_bytes((api.get_source)(filesystem)),
                copied_bytes((api.get_target)(filesystem)),
                copied_string((api.get_fstype)(filesystem)),
                copied_string((api.get_options)(filesystem)),
            )
        };
        let (Some(source), Some(target)) = (source, target) else {
            continue;
        };
        entries.push(LibmountEntry {
            source: OsString::from_vec(source),
            target: PathBuf::from(OsString::from_vec(target)),
            filesystem_type: filesystem_type.unwrap_or_default(),
            options: options
                .map(|value| value.split(',').map(str::to_owned).collect())
                .unwrap_or_default(),
        });
    }
    Ok(entries)
}

/// Owns one libmount table reference.
struct Table<'api, 'library> {
    pointer: *mut TableRaw,
    api: &'api Api<'library>,
}

impl Drop for Table<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: pointer came from `new_table`, is released exactly once, and
        // the function symbol remains live through the borrowed API.
        unsafe { (self.api.free_table)(self.pointer) };
    }
}

/// Owns one libmount iterator reference.
struct Iter<'api, 'library> {
    pointer: *mut IterRaw,
    api: &'api Api<'library>,
}

impl Drop for Iter<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: pointer came from `new_iter`, is released exactly once, and
        // the function symbol remains live through the borrowed API.
        unsafe { (self.api.free_iter)(self.pointer) };
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
