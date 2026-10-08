//! Typed read-only libzfs, libzfs_core, and libnvpair FFI adapter.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_uint, c_void},
    path::PathBuf,
};

use libloading::Symbol;

use super::{LoadedLibrary, load_symbol_from_set};
use crate::{
    NativeProbeError, ZfsDatasetEntry, ZfsDatasetKind, ZfsEntry, ZfsPoolEntry, ZfsVdevClass,
    ZfsVdevEntry, ZfsVdevKind,
};

const ZFS_TYPE_FILESYSTEM: c_int = 1;
const ZFS_TYPE_VOLUME: c_int = 4;
const MAX_NATIVE_ENTRIES: usize = 65_536;

/// Opaque libzfs context.
enum LibzfsRaw {}
/// Opaque ZFS pool handle.
enum PoolRaw {}
/// Opaque ZFS dataset handle.
enum DatasetRaw {}
/// Opaque name/value list shared by the OpenZFS libraries.
enum NvListRaw {}

type Init = unsafe extern "C" fn() -> *mut LibzfsRaw;
type Fini = unsafe extern "C" fn(*mut LibzfsRaw);
type PoolCallback = unsafe extern "C" fn(*mut PoolRaw, *mut c_void) -> c_int;
type PoolIter = unsafe extern "C" fn(*mut LibzfsRaw, PoolCallback, *mut c_void) -> c_int;
type PoolGetName = unsafe extern "C" fn(*mut PoolRaw) -> *const c_char;
type PoolGetConfig = unsafe extern "C" fn(*mut PoolRaw, *mut *mut NvListRaw) -> *mut NvListRaw;
type PoolGetState = unsafe extern "C" fn(*mut PoolRaw) -> c_int;
type PoolClose = unsafe extern "C" fn(*mut PoolRaw);
type DatasetCallback = unsafe extern "C" fn(*mut DatasetRaw, *mut c_void) -> c_int;
type DatasetIterRoot = unsafe extern "C" fn(*mut LibzfsRaw, DatasetCallback, *mut c_void) -> c_int;
type DatasetIterChildren =
    unsafe extern "C" fn(*mut DatasetRaw, DatasetCallback, *mut c_void) -> c_int;
type DatasetGetName = unsafe extern "C" fn(*const DatasetRaw) -> *const c_char;
type DatasetGetType = unsafe extern "C" fn(*const DatasetRaw) -> c_int;
type DatasetClose = unsafe extern "C" fn(*mut DatasetRaw);
type LzcGetProps = unsafe extern "C" fn(*const c_char, *mut *mut NvListRaw) -> c_int;
type NvListFree = unsafe extern "C" fn(*mut NvListRaw);
type LookupNvList =
    unsafe extern "C" fn(*mut NvListRaw, *const c_char, *mut *mut NvListRaw) -> c_int;
type LookupNvListArray = unsafe extern "C" fn(
    *mut NvListRaw,
    *const c_char,
    *mut *mut *mut NvListRaw,
    *mut c_uint,
) -> c_int;
type LookupString = unsafe extern "C" fn(*mut NvListRaw, *const c_char, *mut *mut c_char) -> c_int;
type LookupUint64 = unsafe extern "C" fn(*mut NvListRaw, *const c_char, *mut u64) -> c_int;

/// Symbols used across the three OpenZFS libraries.
struct Api<'library> {
    init: Symbol<'library, Init>,
    fini: Symbol<'library, Fini>,
    pool_iter: Symbol<'library, PoolIter>,
    pool_name: Symbol<'library, PoolGetName>,
    pool_config: Symbol<'library, PoolGetConfig>,
    pool_state: Symbol<'library, PoolGetState>,
    pool_close: Symbol<'library, PoolClose>,
    dataset_iter_root: Symbol<'library, DatasetIterRoot>,
    dataset_iter_children: Symbol<'library, DatasetIterChildren>,
    dataset_name: Symbol<'library, DatasetGetName>,
    dataset_type: Symbol<'library, DatasetGetType>,
    dataset_close: Symbol<'library, DatasetClose>,
    lzc_get_props: Symbol<'library, LzcGetProps>,
    nvlist_free: Symbol<'library, NvListFree>,
    lookup_nvlist: Symbol<'library, LookupNvList>,
    lookup_nvlist_array: Symbol<'library, LookupNvListArray>,
    lookup_string: Symbol<'library, LookupString>,
    lookup_uint64: Symbol<'library, LookupUint64>,
}

impl<'library> Api<'library> {
    /// Resolves the exact libzfs, libzfs_core, and libnvpair ABI in use.
    fn load(libraries: &'library LoadedLibrary) -> Result<Self, NativeProbeError> {
        // SAFETY: every signature matches the installed OpenZFS public headers;
        // returned symbols borrow process-lifetime library handles.
        unsafe {
            Ok(Self {
                init: load_symbol_from_set(libraries, b"libzfs_init\0")?,
                fini: load_symbol_from_set(libraries, b"libzfs_fini\0")?,
                pool_iter: load_symbol_from_set(libraries, b"zpool_iter\0")?,
                pool_name: load_symbol_from_set(libraries, b"zpool_get_name\0")?,
                pool_config: load_symbol_from_set(libraries, b"zpool_get_config\0")?,
                pool_state: load_symbol_from_set(libraries, b"zpool_get_state\0")?,
                pool_close: load_symbol_from_set(libraries, b"zpool_close\0")?,
                dataset_iter_root: load_symbol_from_set(libraries, b"zfs_iter_root\0")?,
                dataset_iter_children: load_symbol_from_set(libraries, b"zfs_iter_children\0")?,
                dataset_name: load_symbol_from_set(libraries, b"zfs_get_name\0")?,
                dataset_type: load_symbol_from_set(libraries, b"zfs_get_type\0")?,
                dataset_close: load_symbol_from_set(libraries, b"zfs_close\0")?,
                lzc_get_props: load_symbol_from_set(libraries, b"lzc_get_props\0")?,
                nvlist_free: load_symbol_from_set(libraries, b"nvlist_free\0")?,
                lookup_nvlist: load_symbol_from_set(libraries, b"nvlist_lookup_nvlist\0")?,
                lookup_nvlist_array: load_symbol_from_set(
                    libraries,
                    b"nvlist_lookup_nvlist_array\0",
                )?,
                lookup_string: load_symbol_from_set(libraries, b"nvlist_lookup_string\0")?,
                lookup_uint64: load_symbol_from_set(libraries, b"nvlist_lookup_uint64\0")?,
            })
        }
    }
}

/// Reads imported pools, vdev trees, filesystems, and volumes.
pub(super) fn probe(libraries: &LoadedLibrary) -> Result<ZfsEntry, NativeProbeError> {
    let api = Api::load(libraries)?;
    // SAFETY: libzfs_init takes no arguments and returns an owned context.
    let pointer = unsafe { (api.init)() };
    if pointer.is_null() {
        return Err(NativeProbeError::AllocationFailed {
            object: "libzfs context",
        });
    }
    let context = Context { pointer, api: &api };

    let mut pools = PoolContext {
        entries: Vec::new(),
        api: &api,
        failed: false,
    };
    // SAFETY: context and callback storage remain live for the synchronous call.
    let result = unsafe {
        (api.pool_iter)(
            context.pointer,
            collect_pool,
            std::ptr::from_mut(&mut pools).cast(),
        )
    };
    if result != 0 || pools.failed {
        return Err(call_failed("zpool_iter", result));
    }

    let mut datasets = DatasetContext {
        entries: Vec::new(),
        api: &api,
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
        pools: pools.entries,
        datasets: datasets.entries,
    })
}

/// Callback storage for pool iteration.
struct PoolContext<'api, 'library> {
    entries: Vec<ZfsPoolEntry>,
    api: &'api Api<'library>,
    failed: bool,
}

/// Copies and closes one pool handle supplied by `zpool_iter`.
unsafe extern "C" fn collect_pool(pool: *mut PoolRaw, opaque: *mut c_void) -> c_int {
    // SAFETY: probe passes a live PoolContext for this synchronous callback.
    let context = unsafe { &mut *opaque.cast::<PoolContext<'_, '_>>() };
    if context.entries.len() == MAX_NATIVE_ENTRIES {
        context.failed = true;
        // SAFETY: callback owns the supplied pool handle.
        unsafe { (context.api.pool_close)(pool) };
        return 0;
    }
    // SAFETY: the callback receives a live pool handle and the returned string
    // is borrowed only while that handle remains open.
    let name = unsafe { copied_string((context.api.pool_name)(pool)) };
    // SAFETY: the callback receives a live pool handle; a null second argument
    // requests the current configuration owned by that handle.
    let config = unsafe { (context.api.pool_config)(pool, std::ptr::null_mut()) };
    // SAFETY: the callback receives a live pool handle and the getter does not
    // retain it beyond this call.
    let state = unsafe { (context.api.pool_state)(pool) };
    if let Some(name) = name {
        if config.is_null() {
            context.failed = true;
        } else {
            let guid = lookup_u64(context.api, config, b"pool_guid\0");
            let mut vdevs = Vec::new();
            if let Some(root) = lookup_nvlist(context.api, config, b"vdev_tree\0") {
                collect_root_vdevs(context.api, root, &mut vdevs);
            }
            context.entries.push(ZfsPoolEntry {
                name,
                guid,
                state,
                vdevs,
            });
        }
    }
    // SAFETY: zpool_iter transfers each callback handle to the callback.
    unsafe { (context.api.pool_close)(pool) };
    0
}

/// Collects allocation, cache, and spare vdev branches below the root nvlist.
fn collect_root_vdevs(api: &Api<'_>, root: *mut NvListRaw, output: &mut Vec<ZfsVdevEntry>) {
    for child in lookup_nvlist_array(api, root, b"children\0") {
        let class = vdev_class(api, child);
        collect_vdev(api, child, None, Some(class), output);
    }
    for child in lookup_nvlist_array(api, root, b"l2cache\0") {
        collect_vdev(api, child, None, Some(ZfsVdevClass::Cache), output);
    }
    for child in lookup_nvlist_array(api, root, b"spares\0") {
        collect_vdev(api, child, None, Some(ZfsVdevClass::Spare), output);
    }
}

/// Recursively flattens one vdev branch while preserving parent GUIDs.
fn collect_vdev(
    api: &Api<'_>,
    vdev: *mut NvListRaw,
    parent_guid: Option<u64>,
    class: Option<ZfsVdevClass>,
    output: &mut Vec<ZfsVdevEntry>,
) {
    if output.len() == MAX_NATIVE_ENTRIES {
        return;
    }
    let guid = lookup_u64(api, vdev, b"guid\0");
    let native_type = lookup_string(api, vdev, b"type\0").unwrap_or_else(|| "unknown".into());
    let parity = lookup_u64(api, vdev, b"nparity\0")
        .and_then(|value| u8::try_from(value).ok())
        .unwrap_or(1);
    let kind = match native_type.as_str() {
        "disk" | "file" => ZfsVdevKind::Leaf,
        "mirror" => ZfsVdevKind::Mirror,
        "raidz" => ZfsVdevKind::RaidZ { parity },
        "draid" => ZfsVdevKind::DRaid {
            parity,
            data_width: lookup_u64(api, vdev, b"draid_ndata\0")
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(1),
            distributed_spares: lookup_u64(api, vdev, b"draid_nspares\0")
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(0),
        },
        other => ZfsVdevKind::Other(other.to_owned()),
    };
    output.push(ZfsVdevEntry {
        guid,
        parent_guid,
        class,
        kind,
        path: lookup_string(api, vdev, b"path\0").map(PathBuf::from),
        size: lookup_u64(api, vdev, b"asize\0"),
        missing: lookup_flag(api, vdev, b"not_present\0"),
        faulted: lookup_flag(api, vdev, b"faulted\0"),
        degraded: lookup_flag(api, vdev, b"degraded\0"),
    });
    for child in lookup_nvlist_array(api, vdev, b"children\0") {
        collect_vdev(api, child, guid, None, output);
    }
}

/// Determines the allocation class of a top-level vdev.
fn vdev_class(api: &Api<'_>, vdev: *mut NvListRaw) -> ZfsVdevClass {
    if lookup_flag(api, vdev, b"is_log\0") {
        return ZfsVdevClass::Log;
    }
    match lookup_string(api, vdev, b"alloc_bias\0").as_deref() {
        Some("special") => ZfsVdevClass::Special,
        Some("dedup") => ZfsVdevClass::Dedup,
        _ => ZfsVdevClass::Data,
    }
}

/// Callback storage for recursive dataset iteration.
struct DatasetContext<'api, 'library> {
    entries: Vec<ZfsDatasetEntry>,
    api: &'api Api<'library>,
    failed: bool,
}

/// Copies, recursively visits, and closes one dataset handle.
unsafe extern "C" fn collect_dataset(dataset: *mut DatasetRaw, opaque: *mut c_void) -> c_int {
    let (api, iter_children, close) = {
        // SAFETY: probe passes this exact live context type.
        let context = unsafe { &*opaque.cast::<DatasetContext<'_, '_>>() };
        (
            context.api,
            *context.api.dataset_iter_children,
            *context.api.dataset_close,
        )
    };
    // SAFETY: getters borrow the live dataset handle.
    let name = unsafe { copied_string((api.dataset_name)(dataset)) };
    // SAFETY: type getter borrows the same live handle.
    let native_kind = unsafe { (api.dataset_type)(dataset) };
    if let Some(name) = name {
        let kind = match native_kind {
            ZFS_TYPE_FILESYSTEM => Some(ZfsDatasetKind::Filesystem),
            ZFS_TYPE_VOLUME => Some(ZfsDatasetKind::Volume),
            _ => None,
        };
        if let Some(kind) = kind {
            // SAFETY: no recursive callback or other reference is active.
            let context = unsafe { &mut *opaque.cast::<DatasetContext<'_, '_>>() };
            if context.entries.len() == MAX_NATIVE_ENTRIES {
                context.failed = true;
            } else {
                context.entries.push(dataset_entry(api, name, kind));
            }
        }
    }
    // SAFETY: recursive iteration is synchronous; context remains live.
    let result = unsafe { iter_children(dataset, collect_dataset, opaque) };
    if result != 0 {
        // SAFETY: recursive iteration has returned, so mutable access is unique.
        let context = unsafe { &mut *opaque.cast::<DatasetContext<'_, '_>>() };
        context.failed = true;
    }
    // SAFETY: iterator callback owns the supplied dataset handle.
    unsafe { close(dataset) };
    0
}

/// Reads selected stable properties through libzfs_core and libnvpair.
fn dataset_entry(api: &Api<'_>, name: String, kind: ZfsDatasetKind) -> ZfsDatasetEntry {
    let Ok(native_name) = CString::new(name.as_str()) else {
        return empty_dataset(name, kind);
    };
    let mut properties = std::ptr::null_mut();
    // SAFETY: name is NUL-terminated and output storage is valid.
    let result = unsafe { (api.lzc_get_props)(native_name.as_ptr(), &mut properties) };
    if result != 0 || properties.is_null() {
        return empty_dataset(name, kind);
    }
    let properties = NvListGuard {
        pointer: properties,
        api,
    };
    ZfsDatasetEntry {
        name,
        kind,
        mountpoint: lookup_property_string(api, properties.pointer, b"mountpoint\0"),
        compression: lookup_property_string(api, properties.pointer, b"compression\0"),
        quota: lookup_property_u64(api, properties.pointer, b"quota\0"),
        volume_size: lookup_property_u64(api, properties.pointer, b"volsize\0"),
        origin: lookup_property_string(api, properties.pointer, b"origin\0")
            .filter(|origin| origin != "-"),
    }
}

/// Creates a dataset observation without optional properties.
fn empty_dataset(name: String, kind: ZfsDatasetKind) -> ZfsDatasetEntry {
    ZfsDatasetEntry {
        name,
        kind,
        mountpoint: None,
        compression: None,
        quota: None,
        volume_size: None,
        origin: None,
    }
}

/// Looks up a nested property nvlist and its string value.
fn lookup_property_string(
    api: &Api<'_>,
    properties: *mut NvListRaw,
    property: &'static [u8],
) -> Option<String> {
    lookup_nvlist(api, properties, property).and_then(|value| lookup_string(api, value, b"value\0"))
}

/// Looks up a nested property nvlist and its integer value.
fn lookup_property_u64(
    api: &Api<'_>,
    properties: *mut NvListRaw,
    property: &'static [u8],
) -> Option<u64> {
    lookup_nvlist(api, properties, property).and_then(|value| lookup_u64(api, value, b"value\0"))
}

/// Borrows one nested nvlist.
fn lookup_nvlist(
    api: &Api<'_>,
    list: *mut NvListRaw,
    name: &'static [u8],
) -> Option<*mut NvListRaw> {
    let mut value = std::ptr::null_mut();
    // SAFETY: list and NUL-terminated key are borrowed; output storage is valid.
    (unsafe { (api.lookup_nvlist)(list, name.as_ptr().cast(), &mut value) } == 0
        && !value.is_null())
    .then_some(value)
}

/// Borrows one nested nvlist array as a bounded slice copy.
fn lookup_nvlist_array(
    api: &Api<'_>,
    list: *mut NvListRaw,
    name: &'static [u8],
) -> Vec<*mut NvListRaw> {
    let mut values = std::ptr::null_mut();
    let mut count = 0;
    // SAFETY: list and key are borrowed; outputs match libnvpair ABI.
    let result =
        unsafe { (api.lookup_nvlist_array)(list, name.as_ptr().cast(), &mut values, &mut count) };
    let count = usize::try_from(count)
        .ok()
        .filter(|count| *count <= MAX_NATIVE_ENTRIES)
        .unwrap_or(0);
    if result != 0 || values.is_null() || count == 0 {
        return Vec::new();
    }
    // SAFETY: libnvpair returned `count` borrowed nvlist pointers.
    unsafe { std::slice::from_raw_parts(values, count) }.to_vec()
}

/// Copies one string nvpair value.
fn lookup_string(api: &Api<'_>, list: *mut NvListRaw, name: &'static [u8]) -> Option<String> {
    let mut value = std::ptr::null_mut();
    // SAFETY: list and key are borrowed; returned string belongs to the nvlist.
    let result = unsafe { (api.lookup_string)(list, name.as_ptr().cast(), &mut value) };
    (result == 0).then(|| {
        // SAFETY: successful lookup returns a live NUL-terminated string.
        unsafe { copied_string(value) }
    })?
}

/// Copies one unsigned integer nvpair value.
fn lookup_u64(api: &Api<'_>, list: *mut NvListRaw, name: &'static [u8]) -> Option<u64> {
    let mut value = 0;
    // SAFETY: list and key are borrowed and output storage is valid.
    (unsafe { (api.lookup_uint64)(list, name.as_ptr().cast(), &mut value) } == 0).then_some(value)
}

/// Interprets an integer nvpair as a boolean flag.
fn lookup_flag(api: &Api<'_>, list: *mut NvListRaw, name: &'static [u8]) -> bool {
    lookup_u64(api, list, name).is_some_and(|value| value != 0)
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

/// Owns an nvlist returned by libzfs_core.
struct NvListGuard<'api, 'library> {
    pointer: *mut NvListRaw,
    api: &'api Api<'library>,
}

impl Drop for NvListGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one successful lzc_get_props allocation.
        unsafe { (self.api.nvlist_free)(self.pointer) };
    }
}

/// Copies an optional native string as lossy UTF-8.
unsafe fn copied_string(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: caller guarantees a valid borrowed NUL-terminated string.
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
