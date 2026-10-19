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

/// Opaque libmount execution context; no namespace or fstab data is imported.
enum ContextRaw {}
/// Exact libmount context constructor ABI.
type NewContext = unsafe extern "C" fn() -> *mut ContextRaw;
/// Exact libmount context destructor ABI.
type FreeContext = unsafe extern "C" fn(*mut ContextRaw);
/// Exact source/target/type/options setter ABI.
type SetText = unsafe extern "C" fn(*mut ContextRaw, *const c_char) -> c_int;
/// Exact boolean or option-mode setter ABI.
type SetFlag = unsafe extern "C" fn(*mut ContextRaw, c_int) -> c_int;
/// Exact synchronous mount/status query ABI.
type ContextCall = unsafe extern "C" fn(*mut ContextRaw) -> c_int;

/// Required write symbols, retained for the context's complete synchronous lifetime.
struct MountApi<'a> {
    /// Context allocator.
    new: Symbol<'a, NewContext>,
    /// Context destructor.
    free: Symbol<'a, FreeContext>,
    /// Source setter.
    source: Symbol<'a, SetText>,
    /// Target setter.
    target: Symbol<'a, SetText>,
    /// Filesystem type setter.
    fstype: Symbol<'a, SetText>,
    /// Explicit options setter.
    options: Symbol<'a, SetText>,
    /// Ambient-configuration and fallback suppression setters with their fixed values.
    flags: Vec<(Symbol<'a, SetFlag>, c_int)>,
    /// Sole mutating entrypoint.
    mount: Symbol<'a, ContextCall>,
    /// Independent libmount completion status.
    status: Symbol<'a, ContextCall>,
    /// Whether the kernel mount syscall was attempted.
    syscall_called: Symbol<'a, ContextCall>,
    /// Whether a forbidden external mount helper ran.
    helper_executed: Symbol<'a, ContextCall>,
    /// Native syscall errno.
    errno: Symbol<'a, ContextCall>,
}
impl<'a> MountApi<'a> {
    /// Requires the exact public libmount context ABI; there is no CLI fallback.
    fn load(library: &'a Library) -> Result<Self, NativeProbeError> {
        // SAFETY: function types match libmount.h; all symbols borrow this retained library.
        unsafe {
            Ok(Self {
                new: load_symbol(library, b"mnt_new_context\0")?,
                free: load_symbol(library, b"mnt_free_context\0")?,
                source: load_symbol(library, b"mnt_context_set_source\0")?,
                target: load_symbol(library, b"mnt_context_set_target\0")?,
                fstype: load_symbol(library, b"mnt_context_set_fstype\0")?,
                options: load_symbol(library, b"mnt_context_set_options\0")?,
                flags: [
                    (
                        b"mnt_context_set_optsmode\0".as_slice(),
                        (1 << 12) | (1 << 1),
                    ),
                    (b"mnt_context_disable_helpers\0".as_slice(), 1),
                    (b"mnt_context_disable_canonicalize\0".as_slice(), 1),
                    (b"mnt_context_disable_mtab\0".as_slice(), 1),
                    (b"mnt_context_enable_rwonly_mount\0".as_slice(), 1),
                ]
                .into_iter()
                .map(|(name, value)| Ok((load_symbol(library, name)?, value)))
                .collect::<Result<_, NativeProbeError>>()?,
                mount: load_symbol(library, b"mnt_context_mount\0")?,
                status: load_symbol(library, b"mnt_context_get_status\0")?,
                syscall_called: load_symbol(library, b"mnt_context_syscall_called\0")?,
                helper_executed: load_symbol(library, b"mnt_context_helper_executed\0")?,
                errno: load_symbol(library, b"mnt_context_get_syscall_errno\0")?,
            })
        }
    }
    /// Allocates a context and rejects any failure to suppress helpers, fstab, mtab or RO fallback.
    fn context(&self) -> Result<MountContextGuard<'_, 'a>, NativeProbeError> {
        // SAFETY: constructor has no arguments and returns an owned opaque context.
        let pointer = unsafe { (self.new)() };
        if pointer.is_null() {
            return Err(NativeProbeError::AllocationFailed {
                object: "libmount context",
            });
        }
        let context = MountContextGuard { pointer, api: self };
        for (setter, value) in &self.flags {
            // SAFETY: live newly allocated context and documented libmount integer flag values.
            check_zero("libmount context policy", unsafe {
                setter(pointer, *value)
            })?;
        }
        Ok(context)
    }
}

/// Owns a context independently of the syscall's outcome, freeing it on every exit.
struct MountContextGuard<'api, 'lib> {
    /// Pointer owned by this guard only.
    pointer: *mut ContextRaw,
    /// Destructor and other symbols outlive the context.
    api: &'api MountApi<'lib>,
}
impl Drop for MountContextGuard<'_, '_> {
    /// Releases exactly the context returned by mnt_new_context.
    fn drop(&mut self) {
        // SAFETY: pointer is uniquely owned; its library and destructor are still live.
        unsafe { (self.api.free)(self.pointer) };
    }
}

/// Turns a libmount nonzero return into a bounded numeric diagnostic.
fn check_zero(operation: &'static str, code: c_int) -> Result<(), NativeProbeError> {
    if code == 0 {
        Ok(())
    } else {
        Err(NativeProbeError::CallFailed { operation, code })
    }
}

/// Readiness resolves every write/status symbol and verifies fixed context configuration without mounting.
pub(super) fn check_mount_support(library: &Library) -> Result<(), NativeProbeError> {
    let api = MountApi::load(library)?;
    let _context = api.context()?;
    Ok(())
}

/// Runs one helper-free mount against retained descriptors in this process's current namespace.
pub(super) fn mount_ext4(
    library: &Library,
    source: &std::fs::File,
    target: &std::fs::File,
) -> Result<(), NativeProbeError> {
    use std::{ffi::CString, os::fd::AsRawFd};
    let api = MountApi::load(library)?;
    let context = api.context()?;
    let source = CString::new(format!("/proc/self/fd/{}", source.as_raw_fd())).map_err(|_| {
        NativeProbeError::InvalidData {
            operation: "source descriptor",
        }
    })?;
    // The slash resolves the pinned directory before move_mount's non-following final lookup.
    let target = CString::new(format!("/proc/self/fd/{}/", target.as_raw_fd())).map_err(|_| {
        NativeProbeError::InvalidData {
            operation: "target descriptor",
        }
    })?;
    let options = CString::new(crate::EXT4_HOST_MOUNT_OPTIONS.join(",")).map_err(|_| {
        NativeProbeError::InvalidData {
            operation: "mount options",
        }
    })?;
    for (setter, text) in [
        (&api.source, source.as_c_str()),
        (&api.target, target.as_c_str()),
        (&api.fstype, c"ext4"),
        (&api.options, options.as_c_str()),
    ] {
        // SAFETY: the context and all terminated input strings live through this synchronous call.
        check_zero("libmount endpoint setup", unsafe {
            setter(context.pointer, text.as_ptr())
        })?;
    }
    // SAFETY: retained source/target descriptors and context remain live; helpers and fallbacks are disabled.
    check_zero("mnt_context_mount", unsafe { (api.mount)(context.pointer) })?;
    // SAFETY: all status getters take the same still-live context and return integer values only.
    let complete = unsafe {
        (api.status)(context.pointer) == 1
            && (api.syscall_called)(context.pointer) == 1
            && (api.helper_executed)(context.pointer) == 0
            && (api.errno)(context.pointer) == 0
    };
    if !complete {
        return Err(NativeProbeError::InvalidData {
            operation: "libmount completion",
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "libmount_write_tests.rs"]
mod write_tests;
