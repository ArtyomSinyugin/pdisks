//! Narrow libblockdev-fs ext4 ABI; a complete recipe is checked before any content mutation.

use super::{
    filesystem::{FILESYSTEM_SESSION, FeaturesRaw},
    load_symbol,
};
use crate::NativeProbeError;
use libloading::{Library, Symbol};
use std::ffi::{c_char, c_int, c_void};

/// Ext4 technology and the MKFS/QUERY modes from the installed blockdev/fs.h ABI.
const EXT4_TECH: c_int = 4;
/// Creation plus independent result queries, without repair or force capabilities.
const CREATE_QUERY: u64 = 1 | (1 << 5);
/// Label, UUID and no-discard must be expressible without arbitrary extra arguments.
const REQUIRED_OPTIONS: c_int = 1 | 2 | 8;

/// Native generic mkfs options, including the zeroed reserved ABI space.
#[repr(C)]
struct MkfsOptionsRaw {
    /// Borrowed optional label string.
    label: *const c_char,
    /// Borrowed canonical UUID string.
    uuid: *const c_char,
    /// Real creation rather than a dry-run acknowledgement.
    dry_run: c_int,
    /// Suppresses discard; the recipe authorizes only filesystem creation.
    no_discard: c_int,
    /// Force is never enabled by this recipe.
    force: c_int,
    /// Partition-table creation suppression is not an ext4 option.
    no_pt: c_int,
    /// Reserved bytes required by the installed ABI.
    reserve: [u8; 32],
}

/// Owned BDFSExt4Info from blockdev/fs/ext.h; copied before info_free.
#[repr(C)]
struct ExtInfoRaw {
    /// Owned label string, empty for an unlabelled filesystem.
    label: *mut c_char,
    /// Owned filesystem UUID string.
    uuid: *mut c_char,
    /// Owned human-readable state, not a runtime completion classification.
    state: *mut c_char,
    /// Filesystem block size in bytes.
    block_size: u64,
    /// Total filesystem blocks.
    block_count: u64,
    /// Observed free blocks.
    free_blocks: u64,
}

/// Only the numeric GError code crosses the public boundary; native messages may expose paths.
#[repr(C)]
struct ErrorRaw {
    /// Error domain quark.
    domain: u32,
    /// Domain-specific native code.
    code: c_int,
    /// GLib-owned diagnostic string released with the error.
    message: *mut c_char,
}

/// Standalone plugin initialization ABI.
type Init = unsafe extern "C" fn() -> c_int;
/// Standalone plugin shutdown ABI.
type Close = unsafe extern "C" fn();
/// Exact filesystem technology availability ABI.
type Tech = unsafe extern "C" fn(c_int, u64, *mut *mut ErrorRaw) -> c_int;
/// Generic mkfs option availability ABI.
type CanMkfs =
    unsafe extern "C" fn(*const c_char, *mut c_int, *mut *mut c_char, *mut *mut ErrorRaw) -> c_int;
/// Static feature/capacity ABI.
type Features = unsafe extern "C" fn(*const c_char, *mut *mut ErrorRaw) -> *const FeaturesRaw;
/// Generic filesystem creation ABI; extra arguments are always NULL.
type Mkfs = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    *mut MkfsOptionsRaw,
    *const *const c_void,
    *mut *mut ErrorRaw,
) -> c_int;
/// Fresh filesystem signature query ABI.
type FsType = unsafe extern "C" fn(*const c_char, *mut *mut ErrorRaw) -> *mut c_char;
/// Fresh ext4 metadata query ABI.
type Info = unsafe extern "C" fn(*const c_char, *mut *mut ErrorRaw) -> *mut ExtInfoRaw;
/// Owned ext4 result release ABI.
type InfoFree = unsafe extern "C" fn(*mut ExtInfoRaw);
/// GLib allocated object release ABI.
type Free = unsafe extern "C" fn(*mut c_void);
/// GLib error release ABI.
type ErrorFree = unsafe extern "C" fn(*mut ErrorRaw);

/// The complete recipe is loaded together; partial symbol availability never permits a write.
struct Api<'library> {
    /// Plugin initialization.
    init: Symbol<'library, Init>,
    /// Plugin shutdown.
    close: Symbol<'library, Close>,
    /// Exact technology support.
    tech: Symbol<'library, Tech>,
    /// Creation options supported on this host.
    can_mkfs: Symbol<'library, CanMkfs>,
    /// Capacity constraints supported by ext4.
    features: Symbol<'library, Features>,
    /// Mutator symbol required even during read-only readiness checks.
    _mkfs: Symbol<'library, Mkfs>,
    /// Signature query required before and after invocation.
    _fs_type: Symbol<'library, FsType>,
    /// Result metadata query required for independent native readback.
    _info: Symbol<'library, Info>,
    /// Matching result release.
    _info_free: Symbol<'library, InfoFree>,
    /// GLib-owned strings.
    free: Symbol<'library, Free>,
    /// GLib-owned errors.
    error_free: Symbol<'library, ErrorFree>,
}

impl<'library> Api<'library> {
    /// Loads the ABI declared by blockdev/fs.h, fs/generic.h and fs/ext.h.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: signatures and repr(C) layouts match the installed headers; all symbols retain the library lifetime.
        unsafe {
            Ok(Self {
                init: load_symbol(library, b"bd_fs_init\0")?,
                close: load_symbol(library, b"bd_fs_close\0")?,
                tech: load_symbol(library, b"bd_fs_is_tech_avail\0")?,
                can_mkfs: load_symbol(library, b"bd_fs_can_mkfs\0")?,
                features: load_symbol(library, b"bd_fs_features\0")?,
                _mkfs: load_symbol(library, b"bd_fs_mkfs\0")?,
                _fs_type: load_symbol(library, b"bd_fs_get_fstype\0")?,
                _info: load_symbol(library, b"bd_fs_ext4_get_info\0")?,
                _info_free: load_symbol(library, b"bd_fs_ext4_info_free\0")?,
                free: load_symbol(library, b"g_free\0")?,
                error_free: load_symbol(library, b"g_error_free\0")?,
            })
        }
    }

    /// Copies a numeric native error before releasing its owned GLib allocation.
    fn check_error(
        &self,
        operation: &'static str,
        error: *mut ErrorRaw,
    ) -> Result<(), NativeProbeError> {
        if error.is_null() {
            return Ok(());
        }
        // SAFETY: the called API returned one owned GError; its code is copied before the matching release.
        let code = unsafe {
            let code = (*error).code;
            (self.error_free)(error);
            code
        };
        Err(NativeProbeError::CallFailed { operation, code })
    }

    /// Checks exact creation/query support and options, returning native ext4 size constraints.
    fn support(&self) -> Result<(u64, u64), NativeProbeError> {
        let mut error = std::ptr::null_mut();
        // SAFETY: initialized plugin, valid scalar technology/mode and live error output storage.
        let available = unsafe { (self.tech)(EXT4_TECH, CREATE_QUERY, &mut error) };
        self.check_error("bd_fs_is_tech_avail", error)?;
        if available == 0 {
            return Err(NativeProbeError::CallFailed {
                operation: "bd_fs_is_tech_avail",
                code: 0,
            });
        }
        let mut options = 0;
        let mut utility = std::ptr::null_mut();
        error = std::ptr::null_mut();
        // SAFETY: ext4 string and all output pointers remain valid throughout this synchronous call.
        let available =
            unsafe { (self.can_mkfs)(c"ext4".as_ptr(), &mut options, &mut utility, &mut error) };
        if !utility.is_null() {
            // SAFETY: can_mkfs returned a transfer-full GLib string.
            unsafe { (self.free)(utility.cast()) };
        }
        self.check_error("bd_fs_can_mkfs", error)?;
        if available == 0 || options & REQUIRED_OPTIONS != REQUIRED_OPTIONS {
            return Err(NativeProbeError::CallFailed {
                operation: "bd_fs_can_mkfs",
                code: 0,
            });
        }
        error = std::ptr::null_mut();
        // SAFETY: ext4 is supported and the returned features are borrowed until this plugin session closes.
        let features = unsafe { (self.features)(c"ext4".as_ptr(), &mut error) };
        self.check_error("bd_fs_features", error)?;
        if features.is_null() {
            return Err(NativeProbeError::InvalidData {
                operation: "bd_fs_features",
            });
        }
        // SAFETY: successful features lookup returned the documented borrowed FeaturesRaw.
        let features = unsafe { &*features };
        Ok((features.min_size, features.max_size))
    }
}

/// Checks the whole native recipe while serializing with ordinary filesystem capability probes.
pub(super) fn check_support(library: &Library) -> Result<(), NativeProbeError> {
    let _lock = FILESYSTEM_SESSION
        .lock()
        .map_err(|_| NativeProbeError::InvalidData {
            operation: "filesystem session lock",
        })?;
    let api = Api::load(library)?;
    // SAFETY: no-argument standalone initialization; success is balanced by Session::drop.
    if unsafe { (api.init)() } == 0 {
        return Err(NativeProbeError::CallFailed {
            operation: "bd_fs_init",
            code: 0,
        });
    }
    let _session = Session(&api);
    api.support().map(|_| ())
}

/// Balances exactly one successful initialization while holding the shared filesystem session lock.
struct Session<'api, 'library>(&'api Api<'library>);
impl Drop for Session<'_, '_> {
    /// Releases plugin global state before the session mutex is unlocked.
    fn drop(&mut self) {
        // SAFETY: this guard exists only after one successful init and borrows the still-loaded API.
        unsafe { (self.0.close)() };
    }
}
