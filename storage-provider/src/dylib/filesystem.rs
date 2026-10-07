//! Typed libblockdev-fs FFI adapter.

use std::ffi::{CStr, CString, c_char, c_int, c_void};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{
    FilesystemCapabilities, FilesystemCreateOptions, FilesystemOperationAvailability,
    FilesystemResizeCapabilities, NativeProbeError,
};

const MAX_NATIVE_ENTRIES: usize = 256;
const RESIZE_OFFLINE_SHRINK: c_int = 1 << 1;
const RESIZE_OFFLINE_GROW: c_int = 1 << 2;
const RESIZE_ONLINE_SHRINK: c_int = 1 << 3;
const RESIZE_ONLINE_GROW: c_int = 1 << 4;
const MKFS_LABEL: c_int = 1 << 0;
const MKFS_UUID: c_int = 1 << 1;
const MKFS_DRY_RUN: c_int = 1 << 2;
const MKFS_NODISCARD: c_int = 1 << 3;
const MKFS_FORCE: c_int = 1 << 4;
const MKFS_NO_PARTITION_TABLE: c_int = 1 << 5;

/// C representation of `BDFSFeatures` consumed by this adapter.
#[repr(C)]
struct FeaturesRaw {
    resize: c_int,
    mkfs: c_int,
    fsck: c_int,
    configure: c_int,
    features: c_int,
    partition_id: *const c_char,
    partition_type: *const c_char,
    min_size: u64,
    max_size: u64,
}

type Init = unsafe extern "C" fn() -> c_int;
type Close = unsafe extern "C" fn();
type SupportedFilesystems = unsafe extern "C" fn(*mut *mut c_void) -> *mut *const c_char;
type Features = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> *const FeaturesRaw;
type CanSimple = unsafe extern "C" fn(*const c_char, *mut *mut c_char, *mut *mut c_void) -> c_int;
type CanMkfs =
    unsafe extern "C" fn(*const c_char, *mut c_int, *mut *mut c_char, *mut *mut c_void) -> c_int;
type CanResize = CanMkfs;
type GFree = unsafe extern "C" fn(*mut c_void);

/// Symbols used by the standalone libblockdev-fs adapter.
struct Api<'library> {
    init: Symbol<'library, Init>,
    close: Symbol<'library, Close>,
    supported: Symbol<'library, SupportedFilesystems>,
    features: Symbol<'library, Features>,
    can_mkfs: Symbol<'library, CanMkfs>,
    can_resize: Symbol<'library, CanResize>,
    can_check: Symbol<'library, CanSimple>,
    can_repair: Symbol<'library, CanSimple>,
    can_set_label: Symbol<'library, CanSimple>,
    can_set_uuid: Symbol<'library, CanSimple>,
    can_get_size: Symbol<'library, CanSimple>,
    can_get_free_space: Symbol<'library, CanSimple>,
    can_get_minimum_size: Symbol<'library, CanSimple>,
    g_free: Symbol<'library, GFree>,
}

impl<'library> Api<'library> {
    /// Resolves the exact libblockdev-fs and GLib ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every function type and `FeaturesRaw` field matches the
        // installed blockdev/fs headers; symbols retain the library lifetime.
        unsafe {
            Ok(Self {
                init: load_symbol(library, b"bd_fs_init\0")?,
                close: load_symbol(library, b"bd_fs_close\0")?,
                supported: load_symbol(library, b"bd_fs_supported_filesystems\0")?,
                features: load_symbol(library, b"bd_fs_features\0")?,
                can_mkfs: load_symbol(library, b"bd_fs_can_mkfs\0")?,
                can_resize: load_symbol(library, b"bd_fs_can_resize\0")?,
                can_check: load_symbol(library, b"bd_fs_can_check\0")?,
                can_repair: load_symbol(library, b"bd_fs_can_repair\0")?,
                can_set_label: load_symbol(library, b"bd_fs_can_set_label\0")?,
                can_set_uuid: load_symbol(library, b"bd_fs_can_set_uuid\0")?,
                can_get_size: load_symbol(library, b"bd_fs_can_get_size\0")?,
                can_get_free_space: load_symbol(library, b"bd_fs_can_get_free_space\0")?,
                can_get_minimum_size: load_symbol(library, b"bd_fs_can_get_min_size\0")?,
                g_free: load_symbol(library, b"g_free\0")?,
            })
        }
    }
}

/// Reports libblockdev filesystem features and their host availability.
pub(super) fn probe_capabilities(
    library: &Library,
) -> Result<Vec<FilesystemCapabilities>, NativeProbeError> {
    let api = Api::load(library)?;
    // SAFETY: standalone plugin initialization takes no arguments.
    if unsafe { (api.init)() } == 0 {
        return Err(NativeProbeError::CallFailed {
            operation: "bd_fs_init",
            code: 0,
        });
    }
    let _session = Session { api: &api };
    // SAFETY: a null GError output is accepted; the returned container is
    // null-terminated and caller-owned while its string elements are static.
    let pointer = unsafe { (api.supported)(std::ptr::null_mut()) };
    if pointer.is_null() {
        return Err(NativeProbeError::AllocationFailed {
            object: "libblockdev filesystem list",
        });
    }
    let array = ArrayGuard {
        pointer: pointer.cast_mut().cast::<c_void>(),
        api: &api,
    };
    let mut capabilities = Vec::new();
    for index in 0..MAX_NATIVE_ENTRIES {
        // SAFETY: the array is documented as null-terminated and remains live.
        let name = unsafe { *pointer.add(index) };
        if name.is_null() {
            drop(array);
            return Ok(capabilities);
        }
        // SAFETY: each array element is a borrowed NUL-terminated string.
        let filesystem = unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned();
        capabilities.push(read_capabilities(&api, &filesystem)?);
    }
    drop(array);
    Err(NativeProbeError::InvalidData {
        operation: "bd_fs_supported_filesystems",
    })
}

/// Copies static and host-specific capabilities for one filesystem.
fn read_capabilities(
    api: &Api<'_>,
    filesystem: &str,
) -> Result<FilesystemCapabilities, NativeProbeError> {
    let name = CString::new(filesystem).map_err(|_| NativeProbeError::InvalidData {
        operation: "bd_fs_supported_filesystems",
    })?;
    // SAFETY: name is NUL-terminated; returned features are library-owned and
    // copied before the plugin session ends.
    let raw = unsafe { (api.features)(name.as_ptr(), std::ptr::null_mut()) };
    if raw.is_null() {
        return Err(NativeProbeError::InvalidData {
            operation: "bd_fs_features",
        });
    }
    let mut mkfs_options = 0;
    let create = availability_mkfs(api, &name, &mut mkfs_options);
    // SAFETY: `raw` remains valid for this initialized plugin session.
    let raw = unsafe { &*raw };
    Ok(FilesystemCapabilities {
        filesystem: filesystem.to_owned(),
        resize: FilesystemResizeCapabilities {
            offline_shrink: raw.resize & RESIZE_OFFLINE_SHRINK != 0,
            offline_grow: raw.resize & RESIZE_OFFLINE_GROW != 0,
            online_shrink: raw.resize & RESIZE_ONLINE_SHRINK != 0,
            online_grow: raw.resize & RESIZE_ONLINE_GROW != 0,
        },
        create_options: FilesystemCreateOptions {
            label: raw.mkfs & MKFS_LABEL != 0,
            uuid: raw.mkfs & MKFS_UUID != 0,
            dry_run: raw.mkfs & MKFS_DRY_RUN != 0,
            no_discard: raw.mkfs & MKFS_NODISCARD != 0,
            force: raw.mkfs & MKFS_FORCE != 0,
            no_partition_table: raw.mkfs & MKFS_NO_PARTITION_TABLE != 0,
        },
        minimum_size: raw.min_size,
        maximum_size: raw.max_size,
        partition_id: copied_string(raw.partition_id),
        partition_type: copied_string(raw.partition_type),
        create,
        resize_operation: availability_with_flags(api, &name, &api.can_resize),
        check: availability_simple(api, &name, &api.can_check),
        repair: availability_simple(api, &name, &api.can_repair),
        set_label: availability_simple(api, &name, &api.can_set_label),
        set_uuid: availability_simple(api, &name, &api.can_set_uuid),
        get_size: availability_simple(api, &name, &api.can_get_size),
        get_free_space: availability_simple(api, &name, &api.can_get_free_space),
        get_minimum_size: availability_simple(api, &name, &api.can_get_minimum_size),
    })
}

/// Calls an availability probe that also reports a feature bitmask.
fn availability_with_flags(
    api: &Api<'_>,
    filesystem: &CString,
    operation: &CanResize,
) -> FilesystemOperationAvailability {
    let mut flags = 0;
    let mut utility = std::ptr::null_mut();
    // SAFETY: all inputs and output storage follow `bd_fs_can_resize`.
    let available = unsafe {
        operation(
            filesystem.as_ptr(),
            &mut flags,
            &mut utility,
            std::ptr::null_mut(),
        ) != 0
    };
    owned_availability(api, available, utility)
}

/// Calls the mkfs availability probe and owns its optional utility string.
fn availability_mkfs(
    api: &Api<'_>,
    filesystem: &CString,
    options: &mut c_int,
) -> FilesystemOperationAvailability {
    let mut utility = std::ptr::null_mut();
    // SAFETY: all inputs and output storage follow `bd_fs_can_mkfs`.
    let available = unsafe {
        (api.can_mkfs)(
            filesystem.as_ptr(),
            options,
            &mut utility,
            std::ptr::null_mut(),
        ) != 0
    };
    owned_availability(api, available, utility)
}

/// Calls one simple availability probe and owns its optional utility string.
fn availability_simple(
    api: &Api<'_>,
    filesystem: &CString,
    operation: &CanSimple,
) -> FilesystemOperationAvailability {
    let mut utility = std::ptr::null_mut();
    // SAFETY: filesystem and output storage follow every `bd_fs_can_*` ABI.
    let available =
        unsafe { operation(filesystem.as_ptr(), &mut utility, std::ptr::null_mut()) != 0 };
    owned_availability(api, available, utility)
}

/// Copies and frees one GLib-owned required-utility string.
fn owned_availability(
    api: &Api<'_>,
    available: bool,
    utility: *mut c_char,
) -> FilesystemOperationAvailability {
    let required_utility = copied_string(utility);
    if !utility.is_null() {
        // SAFETY: libblockdev returned this string with transfer-full ownership.
        unsafe { (api.g_free)(utility.cast()) };
    }
    FilesystemOperationAvailability {
        available,
        required_utility,
    }
}

/// Copies a nullable native string.
fn copied_string(pointer: *const c_char) -> Option<String> {
    (!pointer.is_null()).then(|| {
        // SAFETY: callers only pass strings borrowed from documented C fields.
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    })
}

/// Closes one initialized standalone plugin session.
struct Session<'api, 'library> {
    api: &'api Api<'library>,
}

impl Drop for Session<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one successful `bd_fs_init` call.
        unsafe { (self.api.close)() };
    }
}

/// Owns the container returned by `bd_fs_supported_filesystems`.
struct ArrayGuard<'api, 'library> {
    pointer: *mut c_void,
    api: &'api Api<'library>,
}

impl Drop for ArrayGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: only the GLib-allocated container is caller-owned.
        unsafe { (self.api.g_free)(self.pointer) };
    }
}
