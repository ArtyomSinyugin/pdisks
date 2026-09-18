//! Typed read-only libblockdev LVM-plugin FFI adapter.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    path::PathBuf,
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{LvmEntry, LvmLvEntry, LvmPvEntry, LvmVgEntry, NativeProbeError};

const MAX_NATIVE_ENTRIES: usize = 65_536;

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
}

/// C prefix of `BDLVMVGdata` through the fields consumed here.
#[repr(C)]
struct VgRaw {
    name: *mut c_char,
    uuid: *mut c_char,
    size: u64,
    free: u64,
    extent_size: u64,
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
}

type Init = unsafe extern "C" fn() -> c_int;
type Close = unsafe extern "C" fn();
type Pvs = unsafe extern "C" fn(*mut *mut c_void) -> *mut *mut PvRaw;
type Vgs = unsafe extern "C" fn(*mut *mut c_void) -> *mut *mut VgRaw;
type Lvs = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> *mut *mut LvRaw;
type PvFree = unsafe extern "C" fn(*mut PvRaw);
type VgFree = unsafe extern "C" fn(*mut VgRaw);
type LvFree = unsafe extern "C" fn(*mut LvRaw);
type GFree = unsafe extern "C" fn(*mut c_void);

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
            })
        }
    }
}

/// Reads all visible PVs, VGs, and LVs.
pub(super) fn probe(library: &Library) -> Result<LvmEntry, NativeProbeError> {
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
    // SAFETY: null GError storage is accepted by GLib; the returned outer
    // array and each element are caller-owned.
    let pointer = unsafe { (api.pvs)(std::ptr::null_mut()) };
    if pointer.is_null() {
        return Ok(Vec::new());
    }
    let array = ArrayGuard::new(pointer.cast::<c_void>(), api);
    let mut entries = Vec::new();
    for index in 0..MAX_NATIVE_ENTRIES {
        // SAFETY: array is null-terminated and remains live.
        let raw = unsafe { *pointer.add(index) };
        if raw.is_null() {
            return Ok(entries);
        }
        let item = PvGuard { pointer: raw, api };
        // SAFETY: strings and fields belong to the live item and are copied.
        let (device, uuid) = unsafe {
            (
                copied_string((*item.pointer).pv_name),
                copied_string((*item.pointer).pv_uuid),
            )
        };
        if let (Some(device), Some(uuid)) = (device, uuid) {
            // SAFETY: remaining fields belong to the same live item.
            entries.push(unsafe {
                LvmPvEntry {
                    device: PathBuf::from(device),
                    uuid,
                    vg_name: copied_nonempty((*item.pointer).vg_name),
                    vg_uuid: copied_nonempty((*item.pointer).vg_uuid),
                    size: (*item.pointer).pv_size,
                }
            });
        }
    }
    drop(array);
    Err(invalid("bd_lvm_pvs"))
}

/// Copies the null-terminated VG array.
fn read_vgs(api: &Api<'_>) -> Result<Vec<LvmVgEntry>, NativeProbeError> {
    // SAFETY: null GError storage is accepted by GLib; returned data is owned.
    let pointer = unsafe { (api.vgs)(std::ptr::null_mut()) };
    if pointer.is_null() {
        return Ok(Vec::new());
    }
    let array = ArrayGuard::new(pointer.cast::<c_void>(), api);
    let mut entries = Vec::new();
    for index in 0..MAX_NATIVE_ENTRIES {
        // SAFETY: array is null-terminated and remains live.
        let raw = unsafe { *pointer.add(index) };
        if raw.is_null() {
            return Ok(entries);
        }
        let item = VgGuard { pointer: raw, api };
        // SAFETY: strings and fields belong to the live item and are copied.
        let (name, uuid) = unsafe {
            (
                copied_string((*item.pointer).name),
                copied_string((*item.pointer).uuid),
            )
        };
        if let (Some(name), Some(uuid)) = (name, uuid) {
            // SAFETY: scalar fields belong to the same live item.
            entries.push(unsafe {
                LvmVgEntry {
                    name,
                    uuid,
                    size: (*item.pointer).size,
                    extent_size: (*item.pointer).extent_size,
                }
            });
        }
    }
    drop(array);
    Err(invalid("bd_lvm_vgs"))
}

/// Copies the null-terminated LV array for one VG.
fn read_lvs(api: &Api<'_>, vg_name: &str) -> Result<Vec<LvmLvEntry>, NativeProbeError> {
    let vg_name = CString::new(vg_name).map_err(|_| invalid("bd_lvm_lvs"))?;
    // SAFETY: VG name is NUL-terminated and null GError storage is accepted.
    let pointer = unsafe { (api.lvs)(vg_name.as_ptr(), std::ptr::null_mut()) };
    if pointer.is_null() {
        return Ok(Vec::new());
    }
    let array = ArrayGuard::new(pointer.cast::<c_void>(), api);
    let mut entries = Vec::new();
    for index in 0..MAX_NATIVE_ENTRIES {
        // SAFETY: array is null-terminated and remains live.
        let raw = unsafe { *pointer.add(index) };
        if raw.is_null() {
            return Ok(entries);
        }
        let item = LvGuard { pointer: raw, api };
        // SAFETY: strings and fields belong to the live item and are copied.
        let (name, vg_name, uuid) = unsafe {
            (
                copied_string((*item.pointer).lv_name),
                copied_string((*item.pointer).vg_name),
                copied_string((*item.pointer).uuid),
            )
        };
        if let (Some(name), Some(vg_name), Some(uuid)) = (name, vg_name, uuid) {
            // SAFETY: remaining fields belong to the same live item.
            entries.push(unsafe {
                LvmLvEntry {
                    name,
                    vg_name,
                    uuid,
                    size: (*item.pointer).size,
                    segment_type: copied_nonempty((*item.pointer).segtype),
                }
            });
        }
    }
    drop(array);
    Err(invalid("bd_lvm_lvs"))
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

/// Owns a GLib-allocated outer pointer array.
struct ArrayGuard<'api, 'library> {
    pointer: *mut c_void,
    api: &'api Api<'library>,
}

impl<'api, 'library> ArrayGuard<'api, 'library> {
    /// Wraps one non-null GLib allocation.
    const fn new(pointer: *mut c_void, api: &'api Api<'library>) -> Self {
        Self { pointer, api }
    }
}

impl Drop for ArrayGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: outer array was allocated by GLib and is released once.
        unsafe { (self.api.g_free)(self.pointer) };
    }
}

/// Owns one PV result structure.
struct PvGuard<'api, 'library> {
    pointer: *mut PvRaw,
    api: &'api Api<'library>,
}

impl Drop for PvGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one PV result allocation.
        unsafe { (self.api.pv_free)(self.pointer) };
    }
}

/// Owns one VG result structure.
struct VgGuard<'api, 'library> {
    pointer: *mut VgRaw,
    api: &'api Api<'library>,
}

impl Drop for VgGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one VG result allocation.
        unsafe { (self.api.vg_free)(self.pointer) };
    }
}

/// Owns one LV result structure.
struct LvGuard<'api, 'library> {
    pointer: *mut LvRaw,
    api: &'api Api<'library>,
}

impl Drop for LvGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances one LV result allocation.
        unsafe { (self.api.lv_free)(self.pointer) };
    }
}

/// Copies an optional non-empty native string.
unsafe fn copied_nonempty(pointer: *const c_char) -> Option<String> {
    // SAFETY: caller guarantees a valid optional native string.
    unsafe { copied_string(pointer) }.filter(|value| !value.is_empty())
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

/// Creates the shared error for a GLib boolean failure.
fn call_failed(operation: &'static str) -> NativeProbeError {
    NativeProbeError::CallFailed { operation, code: 0 }
}

/// Creates a malformed-native-data error.
fn invalid(operation: &'static str) -> NativeProbeError {
    NativeProbeError::InvalidData { operation }
}
