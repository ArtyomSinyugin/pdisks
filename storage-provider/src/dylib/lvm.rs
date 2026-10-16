//! Typed read-only libblockdev LVM-plugin FFI adapter.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    path::PathBuf,
    sync::Mutex,
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{LvmEntry, LvmLvEntry, LvmPvEntry, LvmSegmentEntry, LvmVgEntry, NativeProbeError};

const MAX_NATIVE_ENTRIES: usize = 65_536;

/// Standalone plugin init/close operate on process-global LVM state, including across loaded handles.
static LVM_SESSION: Mutex<()> = Mutex::new(());

/// GLib's owned GError prefix; only its numeric code is copied into the public probe error.
#[repr(C)]
struct GErrorRaw {
    /// GLib error domain quark.
    domain: u32,
    /// Domain-specific error code.
    code: c_int,
    /// Owned native message, freed together with the error by g_error_free.
    message: *mut c_char,
}

/// C prefix of `BDLVMPVdata` through the fields consumed here.
#[repr(C)]
struct PvRaw {
    pv_name: *mut c_char,
    pv_uuid: *mut c_char,
    pv_free: u64,
    pv_size: u64,
    pe_start: u64,
    vg_name: *mut c_char,
    vg_uuid: *mut c_char,
    vg_size: u64,
    vg_free: u64,
    vg_extent_size: u64,
    vg_extent_count: u64,
    vg_free_count: u64,
    vg_pv_count: u64,
    pv_tags: *mut *mut c_char,
    missing: c_int,
}

/// C prefix of `BDLVMVGdata` through the fields consumed here.
#[repr(C)]
struct VgRaw {
    name: *mut c_char,
    uuid: *mut c_char,
    size: u64,
    free: u64,
    extent_size: u64,
    extent_count: u64,
    free_count: u64,
    pv_count: u64,
    exported: c_int,
    vg_tags: *mut *mut c_char,
}

/// C representation of one `BDLVMSEGdata` element.
#[repr(C)]
struct SegmentRaw {
    size_pe: u64,
    pv_start_pe: u64,
    pvdev: *mut c_char,
}

/// C prefix of `BDLVMLVdata` through the fields consumed here.
#[repr(C)]
struct LvRaw {
    lv_name: *mut c_char,
    vg_name: *mut c_char,
    uuid: *mut c_char,
    size: u64,
    attr: *mut c_char,
    segtype: *mut c_char,
    origin: *mut c_char,
    pool_lv: *mut c_char,
    data_lv: *mut c_char,
    metadata_lv: *mut c_char,
    roles: *mut c_char,
    move_pv: *mut c_char,
    data_percent: u64,
    metadata_percent: u64,
    copy_percent: u64,
    lv_tags: *mut *mut c_char,
    data_lvs: *mut *mut c_char,
    metadata_lvs: *mut *mut c_char,
    segments: *mut *mut SegmentRaw,
}

type Init = unsafe extern "C" fn() -> c_int;
type Close = unsafe extern "C" fn();
type Pvs = unsafe extern "C" fn(*mut *mut GErrorRaw) -> *mut *mut PvRaw;
type Vgs = unsafe extern "C" fn(*mut *mut GErrorRaw) -> *mut *mut VgRaw;
type Lvs = unsafe extern "C" fn(*const c_char, *mut *mut GErrorRaw) -> *mut *mut LvRaw;
type PvFree = unsafe extern "C" fn(*mut PvRaw);
type VgFree = unsafe extern "C" fn(*mut VgRaw);
type LvFree = unsafe extern "C" fn(*mut LvRaw);
type GFree = unsafe extern "C" fn(*mut c_void);
/// Releases the complete GLib error allocation, including its message.
type GErrorFree = unsafe extern "C" fn(*mut GErrorRaw);
/// Releases one record owned by a terminated native array.
type ItemFree<T> = unsafe extern "C" fn(*mut T);

/// Symbols used by the libblockdev LVM adapter.
struct Api<'library> {
    init: Symbol<'library, Init>,
    close: Symbol<'library, Close>,
    pvs: Symbol<'library, Pvs>,
    vgs: Symbol<'library, Vgs>,
    lvs: Symbol<'library, Lvs>,
    pv_free: Symbol<'library, PvFree>,
    vg_free: Symbol<'library, VgFree>,
    lv_free: Symbol<'library, LvFree>,
    g_free: Symbol<'library, GFree>,
    /// GLib destructor retained with the same loaded library dependency lifetime.
    g_error_free: Symbol<'library, GErrorFree>,
}

impl<'library> Api<'library> {
    /// Resolves the exact LVM-plugin and GLib ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every Rust function and structure prefix matches lvm.h.
        // GLib is a plugin dependency and owns the returned outer arrays.
        unsafe {
            Ok(Self {
                init: load_symbol(library, b"bd_lvm_init\0")?,
                close: load_symbol(library, b"bd_lvm_close\0")?,
                pvs: load_symbol(library, b"bd_lvm_pvs\0")?,
                vgs: load_symbol(library, b"bd_lvm_vgs\0")?,
                lvs: load_symbol(library, b"bd_lvm_lvs\0")?,
                pv_free: load_symbol(library, b"bd_lvm_pvdata_free\0")?,
                vg_free: load_symbol(library, b"bd_lvm_vgdata_free\0")?,
                lv_free: load_symbol(library, b"bd_lvm_lvdata_free\0")?,
                g_free: load_symbol(library, b"g_free\0")?,
                g_error_free: load_symbol(library, b"g_error_free\0")?,
            })
        }
    }
}

/// Reads all visible PVs, VGs, and LVs.
pub(super) fn probe(library: &Library) -> Result<LvmEntry, NativeProbeError> {
    let _exclusive = LVM_SESSION
        .lock()
        .map_err(|_| call_failed("LVM plugin session lock"))?;
    let api = Api::load(library)?;
    // SAFETY: standalone plugin initialization takes no arguments.
    if unsafe { (api.init)() } == 0 {
        return Err(call_failed("bd_lvm_init"));
    }
    let _session = Session { api: &api };
    let pvs = read_pvs(&api)?;
    let vgs = read_vgs(&api)?;
    let mut lvs = Vec::new();
    for vg in &vgs {
        lvs.extend(read_lvs(&api, &vg.name)?);
    }
    Ok(LvmEntry { pvs, vgs, lvs })
}

/// Copies the null-terminated PV array.
fn read_pvs(api: &Api<'_>) -> Result<Vec<LvmPvEntry>, NativeProbeError> {
    let mut error = ErrorGuard::new(&api.g_error_free);
    // SAFETY: error storage is initialized null; the terminated array and all records are owned.
    let pointer = unsafe { (api.pvs)(&mut error.pointer) };
    let _array = ArrayGuard::new(pointer, &api.pv_free, &api.g_free);
    error.check("bd_lvm_pvs")?;
    if pointer.is_null() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for index in 0..MAX_NATIVE_ENTRIES {
        // SAFETY: array is null-terminated and remains live.
        let raw = unsafe { *pointer.add(index) };
        if raw.is_null() {
            return Ok(entries);
        }
        // SAFETY: strings and fields belong to the live item and are copied.
        let (device, uuid) = unsafe {
            (
                copied_required((*raw).pv_name, "bd_lvm_pvs")?,
                copied_required((*raw).pv_uuid, "bd_lvm_pvs")?,
            )
        };
        // SAFETY: remaining fields belong to the same live item.
        entries.push(unsafe {
            LvmPvEntry {
                device: PathBuf::from(device),
                uuid,
                vg_name: copied_nonempty((*raw).vg_name),
                vg_uuid: copied_nonempty((*raw).vg_uuid),
                size: (*raw).pv_size,
                free: (*raw).pv_free,
                data_offset: (*raw).pe_start,
                missing: (*raw).missing != 0,
            }
        });
    }
    Err(invalid("bd_lvm_pvs"))
}

/// Copies the null-terminated VG array.
fn read_vgs(api: &Api<'_>) -> Result<Vec<LvmVgEntry>, NativeProbeError> {
    let mut error = ErrorGuard::new(&api.g_error_free);
    // SAFETY: error storage is initialized null; the terminated array and all records are owned.
    let pointer = unsafe { (api.vgs)(&mut error.pointer) };
    let _array = ArrayGuard::new(pointer, &api.vg_free, &api.g_free);
    error.check("bd_lvm_vgs")?;
    if pointer.is_null() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for index in 0..MAX_NATIVE_ENTRIES {
        // SAFETY: array is null-terminated and remains live.
        let raw = unsafe { *pointer.add(index) };
        if raw.is_null() {
            return Ok(entries);
        }
        // SAFETY: strings and fields belong to the live item and are copied.
        let (name, uuid) = unsafe {
            (
                copied_required((*raw).name, "bd_lvm_vgs")?,
                copied_required((*raw).uuid, "bd_lvm_vgs")?,
            )
        };
        // SAFETY: scalar fields belong to the same live item.
        entries.push(unsafe {
            LvmVgEntry {
                name,
                uuid,
                size: (*raw).size,
                extent_size: (*raw).extent_size,
                free: (*raw).free,
                pv_count: (*raw).pv_count,
                exported: (*raw).exported != 0,
            }
        });
    }
    Err(invalid("bd_lvm_vgs"))
}

/// Copies the null-terminated LV array for one VG.
fn read_lvs(api: &Api<'_>, vg_name: &str) -> Result<Vec<LvmLvEntry>, NativeProbeError> {
    let vg_name = CString::new(vg_name).map_err(|_| invalid("bd_lvm_lvs"))?;
    let mut error = ErrorGuard::new(&api.g_error_free);
    // SAFETY: VG name is terminated and error storage initialized null; returned records are owned.
    let pointer = unsafe { (api.lvs)(vg_name.as_ptr(), &mut error.pointer) };
    let _array = ArrayGuard::new(pointer, &api.lv_free, &api.g_free);
    error.check("bd_lvm_lvs")?;
    if pointer.is_null() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for index in 0..MAX_NATIVE_ENTRIES {
        // SAFETY: array is null-terminated and remains live.
        let raw = unsafe { *pointer.add(index) };
        if raw.is_null() {
            return Ok(entries);
        }
        // SAFETY: strings and fields belong to the live item and are copied.
        let (name, vg_name, uuid) = unsafe {
            (
                copied_required((*raw).lv_name, "bd_lvm_lvs")?,
                copied_required((*raw).vg_name, "bd_lvm_lvs")?,
                copied_required((*raw).uuid, "bd_lvm_lvs")?,
            )
        };
        // SAFETY: remaining fields belong to the same live item.
        entries.push(unsafe {
            LvmLvEntry {
                name,
                vg_name,
                uuid,
                size: (*raw).size,
                segment_type: copied_nonempty((*raw).segtype),
                attributes: copied_nonempty((*raw).attr),
                origin: copied_nonempty((*raw).origin),
                pool: copied_nonempty((*raw).pool_lv),
                data: copied_nonempty((*raw).data_lv),
                metadata: copied_nonempty((*raw).metadata_lv),
                roles: copied_nonempty((*raw).roles),
                data_percent: (*raw).data_percent,
                metadata_percent: (*raw).metadata_percent,
                copy_percent: (*raw).copy_percent,
                segments: copy_segments((*raw).segments)?,
            }
        });
    }
    Err(invalid("bd_lvm_lvs"))
}

/// Copies a null-terminated segment array owned by its parent LV result.
unsafe fn copy_segments(
    pointer: *mut *mut SegmentRaw,
) -> Result<Vec<LvmSegmentEntry>, NativeProbeError> {
    if pointer.is_null() {
        return Ok(Vec::new());
    }
    let mut segments = Vec::new();
    for index in 0..MAX_NATIVE_ENTRIES {
        // SAFETY: caller guarantees the parent LV and its terminated segment
        // array remain live while this function copies fields.
        let raw = unsafe { *pointer.add(index) };
        if raw.is_null() {
            return Ok(segments);
        }
        // SAFETY: raw points to one live segment owned by the parent LV.
        segments.push(unsafe {
            LvmSegmentEntry {
                size_extents: (*raw).size_pe,
                start_extent: (*raw).pv_start_pe,
                device: copied_nonempty((*raw).pvdev).map(PathBuf::from),
            }
        });
    }
    Err(invalid("bd_lvm_lvs segments"))
}

/// Closes one initialized standalone plugin session.
struct Session<'api, 'library> {
    api: &'api Api<'library>,
}

impl Drop for Session<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one successful `bd_lvm_init` call.
        unsafe { (self.api.close)() };
    }
}

/// Owns all records and the outer native array, including unread records after a parse error.
struct ArrayGuard<'function, T> {
    /// Terminated native allocation, or null for a successful empty result.
    pointer: *mut *mut T,
    /// Type-correct record destructor borrowed from the live API symbol.
    free_item: &'function ItemFree<T>,
    /// GLib destructor for the outer allocation.
    free_array: &'function GFree,
}

impl<'function, T> ArrayGuard<'function, T> {
    /// Wraps the complete caller-owned result before checking errors or parsing fields.
    fn new(
        pointer: *mut *mut T,
        free_item: &'function ItemFree<T>,
        free_array: &'function GFree,
    ) -> Self {
        Self {
            pointer,
            free_item,
            free_array,
        }
    }
}

impl<T> Drop for ArrayGuard<'_, T> {
    /// Frees even unvisited records exactly once while all borrowed destructor symbols remain live.
    fn drop(&mut self) {
        if self.pointer.is_null() {
            return;
        }
        // Parse loops are bounded; cleanup must release the complete terminated native allocation.
        for index in 0.. {
            // SAFETY: the library returned a terminated array; each entry is uniquely owned here.
            let record = unsafe { *self.pointer.add(index) };
            if record.is_null() {
                break;
            }
            // SAFETY: this matching native destructor owns this record and its nested allocations.
            unsafe { (self.free_item)(record) };
        }
        // SAFETY: the complete outer array is a GLib allocation, freed once after its records.
        unsafe { (self.free_array)(self.pointer.cast()) };
    }
}

/// Owns one optional GLib error; a null data result alone cannot distinguish failure from empty state.
struct ErrorGuard<'function> {
    /// Initialized null before crossing the FFI boundary, then exclusively owned if populated.
    pointer: *mut GErrorRaw,
    /// Destructor borrowed from the live GLib dependency symbol.
    free: &'function GErrorFree,
}

impl<'function> ErrorGuard<'function> {
    /// Supplies valid, initially empty GError storage for exactly one native call.
    fn new(free: &'function GErrorFree) -> Self {
        Self {
            pointer: std::ptr::null_mut(),
            free,
        }
    }
    /// Reports native failure without converting it into an empty observation or trusting message text.
    fn check(&self, operation: &'static str) -> Result<(), NativeProbeError> {
        if self.pointer.is_null() {
            return Ok(());
        }
        // SAFETY: a non-null pointer was returned as an owned live GError by the matching API.
        let code = unsafe { (*self.pointer).code };
        Err(NativeProbeError::CallFailed { operation, code })
    }
}

impl Drop for ErrorGuard<'_> {
    /// Releases an owned native error even if result validation exits early.
    fn drop(&mut self) {
        if !self.pointer.is_null() {
            // SAFETY: the matching GLib destructor owns the returned error and message exactly once.
            unsafe { (self.free)(self.pointer) };
        }
    }
}

/// Rejects absent, empty, or invalid UTF-8 identities instead of silently dropping an observed record.
unsafe fn copied_required(
    pointer: *const c_char,
    operation: &'static str,
) -> Result<String, NativeProbeError> {
    // SAFETY: the caller guarantees an optional terminated string borrowed from a live native record.
    unsafe { copied_nonempty(pointer) }.ok_or_else(|| invalid(operation))
}

/// Copies an optional non-empty native string.
unsafe fn copied_nonempty(pointer: *const c_char) -> Option<String> {
    // SAFETY: caller guarantees a valid optional native string.
    unsafe { copied_string(pointer) }.filter(|value| !value.is_empty())
}

/// Copies an optional native string without changing invalid UTF-8 into a different identity.
unsafe fn copied_string(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: caller guarantees a valid NUL-terminated borrowed C string.
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// Creates the shared error for a GLib boolean failure.
fn call_failed(operation: &'static str) -> NativeProbeError {
    NativeProbeError::CallFailed { operation, code: 0 }
}

/// Creates a malformed-native-data error.
fn invalid(operation: &'static str) -> NativeProbeError {
    NativeProbeError::InvalidData { operation }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Number of simulated GError releases; only the error-guard test uses this counter.
    static ERRORS_FREED: AtomicUsize = AtomicUsize::new(0);
    /// Sum of uniquely freed fixture record values; catches skipped or duplicated destructors.
    static RECORDS_FREED: AtomicUsize = AtomicUsize::new(0);
    /// Number of outer-array release calls in the ownership test.
    static ARRAYS_FREED: AtomicUsize = AtomicUsize::new(0);

    /// Mock error destructor observes release of a stack-owned fixture without deallocating it.
    extern "C" fn free_error(_pointer: *mut GErrorRaw) {
        ERRORS_FREED.fetch_add(1, Ordering::SeqCst);
    }

    /// A native numeric error is preserved and released; no-error empty output remains distinguishable.
    #[test]
    fn glib_error_is_not_an_empty_result_and_is_released_once() {
        ERRORS_FREED.store(0, Ordering::SeqCst);
        let free: GErrorFree = free_error;
        {
            let guard = ErrorGuard::new(&free);
            assert!(guard.check("bd_lvm_pvs").is_ok());
        }
        assert_eq!(ERRORS_FREED.load(Ordering::SeqCst), 0);
        let mut raw = GErrorRaw {
            domain: 1,
            code: 17,
            message: c"native failure".as_ptr().cast_mut(),
        };
        {
            let mut guard = ErrorGuard::new(&free);
            guard.pointer = &mut raw;
            assert!(matches!(
                guard.check("bd_lvm_lvs"),
                Err(NativeProbeError::CallFailed {
                    operation: "bd_lvm_lvs",
                    code: 17
                })
            ));
        }
        assert_eq!(ERRORS_FREED.load(Ordering::SeqCst), 1);
    }

    /// Invalid required strings cannot silently omit a PV/VG/LV or turn into another UTF-8 identity.
    #[test]
    fn required_native_identity_rejects_null_empty_and_invalid_utf8()
    -> Result<(), Box<dyn std::error::Error>> {
        let invalid_utf8 = CString::new(vec![0xff])?;
        for pointer in [std::ptr::null(), c"".as_ptr(), invalid_utf8.as_ptr()] {
            // SAFETY: each non-null pointer is a live terminated fixture string for this call.
            assert!(unsafe { copied_required(pointer, "bd_lvm_pvs") }.is_err());
        }
        // SAFETY: the fixture is a live static terminated C string.
        let copied = unsafe { copied_required(c"pv-uuid".as_ptr(), "bd_lvm_pvs") }?;
        assert_eq!(copied, "pv-uuid");
        Ok(())
    }

    /// Minimal C-layout record used only by the array ownership test.
    #[repr(C)]
    struct Record {
        /// Value summed when its allocation is released.
        value: usize,
    }

    /// Mock native destructor receives exactly the Box allocation owned by one array element.
    extern "C" fn free_record(pointer: *mut Record) {
        // SAFETY: the test supplies each Box::into_raw pointer once to its sole array owner.
        let record = unsafe { Box::from_raw(pointer) };
        RECORDS_FREED.fetch_add(record.value, Ordering::SeqCst);
    }

    /// The test owns the array storage itself; this mock only observes the native outer-free call.
    extern "C" fn free_array(_pointer: *mut c_void) {
        ARRAYS_FREED.fetch_add(1, Ordering::SeqCst);
    }

    /// A required-field parse error still frees every unvisited record and the outer native array.
    #[test]
    fn array_guard_cleans_unvisited_records_on_parse_failure() {
        RECORDS_FREED.store(0, Ordering::SeqCst);
        ARRAYS_FREED.store(0, Ordering::SeqCst);
        let mut pointers = [
            Box::into_raw(Box::new(Record { value: 2 })),
            Box::into_raw(Box::new(Record { value: 5 })),
            std::ptr::null_mut(),
        ];
        let item_free: ItemFree<Record> = free_record;
        let array_free: GFree = free_array;
        let result = (|| {
            let _guard = ArrayGuard::new(pointers.as_mut_ptr(), &item_free, &array_free);
            // SAFETY: null is the documented missing optional native string representation.
            unsafe { copied_required(std::ptr::null(), "bd_lvm_vgs") }?;
            Ok::<_, NativeProbeError>(())
        })();
        assert!(result.is_err());
        assert_eq!(RECORDS_FREED.load(Ordering::SeqCst), 7);
        assert_eq!(ARRAYS_FREED.load(Ordering::SeqCst), 1);
    }
}
