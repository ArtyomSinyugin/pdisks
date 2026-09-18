//! Typed read-only libzfs FFI adapter.

use std::ffi::{CStr, c_char, c_int, c_void};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{NativeProbeError, ZfsDatasetEntry, ZfsDatasetKind, ZfsEntry};

const ZFS_TYPE_FILESYSTEM: c_int = 1;
const ZFS_TYPE_VOLUME: c_int = 4;
const MAX_NATIVE_ENTRIES: usize = 65_536;

/// Opaque libzfs context.
enum LibzfsRaw {}
/// Opaque ZFS pool handle.
enum PoolRaw {}
/// Opaque ZFS dataset handle.
enum DatasetRaw {}

type Init = unsafe extern "C" fn() -> *mut LibzfsRaw;
type Fini = unsafe extern "C" fn(*mut LibzfsRaw);
type PoolCallback = unsafe extern "C" fn(*mut PoolRaw, *mut c_void) -> c_int;
type PoolIter = unsafe extern "C" fn(*mut LibzfsRaw, PoolCallback, *mut c_void) -> c_int;
type PoolGetName = unsafe extern "C" fn(*mut PoolRaw) -> *const c_char;
type PoolClose = unsafe extern "C" fn(*mut PoolRaw);
type DatasetCallback = unsafe extern "C" fn(*mut DatasetRaw, *mut c_void) -> c_int;
type DatasetIterRoot = unsafe extern "C" fn(*mut LibzfsRaw, DatasetCallback, *mut c_void) -> c_int;
type DatasetIterChildren =
    unsafe extern "C" fn(*mut DatasetRaw, DatasetCallback, *mut c_void) -> c_int;
type DatasetGetName = unsafe extern "C" fn(*const DatasetRaw) -> *const c_char;
type DatasetGetType = unsafe extern "C" fn(*const DatasetRaw) -> c_int;
type DatasetClose = unsafe extern "C" fn(*mut DatasetRaw);

/// Symbols used by the libzfs adapter.
struct Api<'library> {
    init: Symbol<'library, Init>,
    fini: Symbol<'library, Fini>,
    pool_iter: Symbol<'library, PoolIter>,
    pool_name: Symbol<'library, PoolGetName>,
    pool_close: Symbol<'library, PoolClose>,
    dataset_iter_root: Symbol<'library, DatasetIterRoot>,
    dataset_iter_children: Symbol<'library, DatasetIterChildren>,
    dataset_name: Symbol<'library, DatasetGetName>,
    dataset_type: Symbol<'library, DatasetGetType>,
    dataset_close: Symbol<'library, DatasetClose>,
}

impl<'library> Api<'library> {
    /// Resolves the exact libzfs ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: function types match libzfs.h and symbols retain the library
        // lifetime.
        unsafe {
            Ok(Self {
                init: load_symbol(library, b"libzfs_init\0")?,
                fini: load_symbol(library, b"libzfs_fini\0")?,
                pool_iter: load_symbol(library, b"zpool_iter\0")?,
                pool_name: load_symbol(library, b"zpool_get_name\0")?,
                pool_close: load_symbol(library, b"zpool_close\0")?,
                dataset_iter_root: load_symbol(library, b"zfs_iter_root\0")?,
                dataset_iter_children: load_symbol(library, b"zfs_iter_children\0")?,
                dataset_name: load_symbol(library, b"zfs_get_name\0")?,
                dataset_type: load_symbol(library, b"zfs_get_type\0")?,
                dataset_close: load_symbol(library, b"zfs_close\0")?,
            })
        }
    }
}

/// Reads imported pools, filesystems, and volumes.
pub(super) fn probe(library: &Library) -> Result<ZfsEntry, NativeProbeError> {
    let api = Api::load(library)?;
    // SAFETY: libzfs_init takes no arguments and returns an owned context.
    let pointer = unsafe { (api.init)() };
    if pointer.is_null() {
        // A host without a loaded ZFS kernel module has no observable ZFS
        // topology; libzfs reports that state as a null initialization handle.
        return Ok(ZfsEntry::default());
    }
    let context = Context { pointer, api: &api };

    let mut pools = PoolContext {
        names: Vec::new(),
        get_name: *api.pool_name,
        close: *api.pool_close,
    };
    // SAFETY: context and callback storage remain live for the synchronous call.
    let result = unsafe {
        (api.pool_iter)(
            context.pointer,
            collect_pool,
            std::ptr::from_mut(&mut pools).cast(),
        )
    };
    if result != 0 {
        return Err(call_failed("zpool_iter", result));
    }

    let mut datasets = DatasetContext {
        entries: Vec::new(),
        iter_children: *api.dataset_iter_children,
        get_name: *api.dataset_name,
        get_type: *api.dataset_type,
        close: *api.dataset_close,
        failed: false,
    };
    // SAFETY: context and callback storage remain live for the synchronous call.
    let result = unsafe {
        (api.dataset_iter_root)(
            context.pointer,
            collect_dataset,
            std::ptr::from_mut(&mut datasets).cast(),
        )
    };
    if result != 0 || datasets.failed {
        return Err(call_failed("zfs_iter_root", result));
    }
    Ok(ZfsEntry {
        pools: pools.names,
        datasets: datasets.entries,
    })
}

/// Callback storage for pool iteration.
struct PoolContext {
    names: Vec<String>,
    get_name: PoolGetName,
    close: PoolClose,
}

/// Copies and closes one pool handle supplied by zpool_iter.
unsafe extern "C" fn collect_pool(pool: *mut PoolRaw, opaque: *mut c_void) -> c_int {
    // SAFETY: probe passes a live PoolContext for this synchronous callback.
    let context = unsafe { &mut *opaque.cast::<PoolContext>() };
    // SAFETY: pool is live for the callback and owns its name string.
    let name = unsafe { copied_string((context.get_name)(pool)) };
    if let Some(name) = name
        && context.names.len() < MAX_NATIVE_ENTRIES
    {
        context.names.push(name);
    }
    // SAFETY: zpool_iter transfers each callback handle to the callback.
    unsafe { (context.close)(pool) };
    0
}

/// Callback storage for recursive dataset iteration.
struct DatasetContext {
    entries: Vec<ZfsDatasetEntry>,
    iter_children: DatasetIterChildren,
    get_name: DatasetGetName,
    get_type: DatasetGetType,
    close: DatasetClose,
    failed: bool,
}

/// Copies, recursively visits, and closes one dataset handle.
unsafe extern "C" fn collect_dataset(dataset: *mut DatasetRaw, opaque: *mut c_void) -> c_int {
    // SAFETY: probe passes a live DatasetContext for synchronous callbacks.
    let (iter_children, get_name, get_type, close) = {
        // SAFETY: the opaque pointer was created from this exact context type.
        let context = unsafe { &*opaque.cast::<DatasetContext>() };
        (
            context.iter_children,
            context.get_name,
            context.get_type,
            context.close,
        )
    };
    // SAFETY: getters borrow the live dataset handle.
    let (name, kind) = unsafe { (copied_string(get_name(dataset)), get_type(dataset)) };
    if let Some(name) = name {
        let kind = match kind {
            ZFS_TYPE_FILESYSTEM => Some(ZfsDatasetKind::Filesystem),
            ZFS_TYPE_VOLUME => Some(ZfsDatasetKind::Volume),
            _ => None,
        };
        if let Some(kind) = kind {
            // SAFETY: no recursive callback is active while this mutable
            // reference is held.
            let context = unsafe { &mut *opaque.cast::<DatasetContext>() };
            if context.entries.len() == MAX_NATIVE_ENTRIES {
                context.failed = true;
            } else {
                context.entries.push(ZfsDatasetEntry { name, kind });
            }
        }
    }
    // SAFETY: recursion is synchronous. No Rust reference to the callback
    // context remains live across this reentrant call.
    let result = unsafe { iter_children(dataset, collect_dataset, opaque) };
    if result != 0 {
        // SAFETY: the recursive call has returned, so mutable access is unique.
        let context = unsafe { &mut *opaque.cast::<DatasetContext>() };
        context.failed = true;
    }
    // SAFETY: iterator callbacks own the supplied dataset handle.
    unsafe { close(dataset) };
    0
}

/// Owns one initialized libzfs context.
struct Context<'api, 'library> {
    pointer: *mut LibzfsRaw,
    api: &'api Api<'library>,
}

impl Drop for Context<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one successful libzfs_init call.
        unsafe { (self.api.fini)(self.pointer) };
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

/// Creates the shared native call error.
fn call_failed(operation: &'static str, code: c_int) -> NativeProbeError {
    NativeProbeError::CallFailed { operation, code }
}
