//! Typed read-only libbtrfsutil FFI adapter.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{BtrfsEntry, BtrfsSubvolumeEntry, NativeProbeError};

const BTRFS_FS_TREE_OBJECTID: u64 = 5;
const BTRFS_UTIL_OK: c_int = 0;
const BTRFS_UTIL_ERROR_STOP_ITERATION: c_int = 1;
const MAX_NATIVE_ENTRIES: usize = 65_536;

/// Opaque libbtrfsutil subvolume iterator.
enum IteratorRaw {}

type CreateIterator =
    unsafe extern "C" fn(*const c_char, u64, c_int, *mut *mut IteratorRaw) -> c_int;
type DestroyIterator = unsafe extern "C" fn(*mut IteratorRaw);
type NextIterator = unsafe extern "C" fn(*mut IteratorRaw, *mut *mut c_char, *mut u64) -> c_int;
type GetReadOnly = unsafe extern "C" fn(*const c_char, *mut bool) -> c_int;
type GetDefault = unsafe extern "C" fn(*const c_char, *mut u64) -> c_int;
type Free = unsafe extern "C" fn(*mut c_void);

/// Symbols used by the libbtrfsutil adapter.
struct Api<'library> {
    create: Symbol<'library, CreateIterator>,
    destroy: Symbol<'library, DestroyIterator>,
    next: Symbol<'library, NextIterator>,
    get_read_only: Symbol<'library, GetReadOnly>,
    get_default: Symbol<'library, GetDefault>,
    free: Symbol<'library, Free>,
}

impl<'library> Api<'library> {
    /// Resolves the exact libbtrfsutil ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every function type matches btrfsutil.h; symbols retain the
        // library lifetime. `free` releases paths documented as malloc-owned.
        unsafe {
            Ok(Self {
                create: load_symbol(library, b"btrfs_util_create_subvolume_iterator\0")?,
                destroy: load_symbol(library, b"btrfs_util_destroy_subvolume_iterator\0")?,
                next: load_symbol(library, b"btrfs_util_subvolume_iterator_next\0")?,
                get_read_only: load_symbol(library, b"btrfs_util_get_subvolume_read_only\0")?,
                get_default: load_symbol(library, b"btrfs_util_get_default_subvolume\0")?,
                free: load_symbol(library, b"free\0")?,
            })
        }
    }
}

/// Lists subvolumes below every supplied mounted Btrfs filesystem.
pub(super) fn probe(
    library: &Library,
    mountpoints: &[PathBuf],
) -> Result<Vec<BtrfsEntry>, NativeProbeError> {
    let api = Api::load(library)?;
    mountpoints
        .iter()
        .map(|mountpoint| inspect_mount(&api, mountpoint))
        .collect()
}

/// Reads one mounted filesystem through a bounded native iterator.
fn inspect_mount(api: &Api<'_>, mountpoint: &Path) -> Result<BtrfsEntry, NativeProbeError> {
    let path = CString::new(mountpoint.as_os_str().as_bytes()).map_err(|_| {
        NativeProbeError::InvalidPath {
            path: mountpoint.to_path_buf(),
        }
    })?;
    let mut pointer = std::ptr::null_mut();
    // SAFETY: path is NUL-terminated and output storage is valid. Object ID 5
    // requests all filesystem subvolumes, with no iterator flags.
    let result = unsafe { (api.create)(path.as_ptr(), BTRFS_FS_TREE_OBJECTID, 0, &mut pointer) };
    if result != BTRFS_UTIL_OK || pointer.is_null() {
        return Err(NativeProbeError::CallFailed {
            operation: "btrfs_util_create_subvolume_iterator",
            code: result,
        });
    }
    let iterator = IteratorGuard { pointer, api };
    let mut default_id = 0;
    // SAFETY: path points into the live CString and output storage is valid.
    let result = unsafe { (api.get_default)(path.as_ptr(), &mut default_id) };
    if result != BTRFS_UTIL_OK {
        return Err(NativeProbeError::CallFailed {
            operation: "btrfs_util_get_default_subvolume",
            code: result,
        });
    }
    let mut subvolumes = Vec::new();
    for _ in 0..MAX_NATIVE_ENTRIES {
        let mut native_path = std::ptr::null_mut();
        let mut id = 0;
        // SAFETY: iterator is live and both output pointers remain valid.
        let result = unsafe { (api.next)(iterator.pointer, &mut native_path, &mut id) };
        if result == BTRFS_UTIL_ERROR_STOP_ITERATION {
            return Ok(BtrfsEntry {
                mountpoint: mountpoint.to_path_buf(),
                subvolumes,
            });
        }
        if result != BTRFS_UTIL_OK || native_path.is_null() {
            return Err(NativeProbeError::CallFailed {
                operation: "btrfs_util_subvolume_iterator_next",
                code: result,
            });
        }
        let owned_path = NativePath {
            pointer: native_path,
            api,
        };
        // SAFETY: the iterator returned a live NUL-terminated path.
        let bytes = unsafe { CStr::from_ptr(owned_path.pointer) }
            .to_bytes()
            .to_vec();
        let relative_path = PathBuf::from(std::ffi::OsString::from_vec(bytes));
        let absolute_path = mountpoint.join(&relative_path);
        let native_absolute = CString::new(absolute_path.as_os_str().as_bytes()).map_err(|_| {
            NativeProbeError::InvalidPath {
                path: absolute_path.clone(),
            }
        })?;
        let mut read_only = false;
        // SAFETY: path is NUL-terminated and output storage is valid.
        let result = unsafe { (api.get_read_only)(native_absolute.as_ptr(), &mut read_only) };
        if result != BTRFS_UTIL_OK {
            return Err(NativeProbeError::CallFailed {
                operation: "btrfs_util_get_subvolume_read_only",
                code: result,
            });
        }
        subvolumes.push(BtrfsSubvolumeEntry {
            id,
            path: relative_path,
            read_only,
            is_default: id == default_id,
        });
    }
    Err(NativeProbeError::InvalidData {
        operation: "btrfs_util_subvolume_iterator_next",
    })
}

/// Owns one libbtrfsutil iterator.
struct IteratorGuard<'api, 'library> {
    pointer: *mut IteratorRaw,
    api: &'api Api<'library>,
}

impl Drop for IteratorGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one successful iterator creation.
        unsafe { (self.api.destroy)(self.pointer) };
    }
}

/// Owns one malloc-allocated subvolume path.
struct NativePath<'api, 'library> {
    pointer: *mut c_char,
    api: &'api Api<'library>,
}

impl Drop for NativePath<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: libbtrfsutil documents iterator paths as free()-owned.
        unsafe { (self.api.free)(self.pointer.cast()) };
    }
}
