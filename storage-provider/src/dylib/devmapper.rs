//! Typed read-only libdevmapper FFI adapter.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr,
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{DevmapperEntry, NativeDeviceNumber, NativeProbeError};

const DM_DEVICE_DEPS: c_int = 7;
const DM_DEVICE_TABLE: c_int = 11;
const DM_DEVICE_LIST: c_int = 13;
const MAX_NATIVE_ENTRIES: usize = 65_536;

/// Opaque libdevmapper task.
enum TaskRaw {}

/// C layout of `struct dm_info`.
#[repr(C)]
#[derive(Default)]
struct DmInfo {
    exists: c_int,
    suspended: c_int,
    live_table: c_int,
    inactive_table: c_int,
    open_count: i32,
    event_nr: u32,
    major: u32,
    minor: u32,
    read_only: c_int,
    target_count: i32,
    deferred_remove: c_int,
    internal_suspend: c_int,
}

/// C layout of the fixed prefix of `struct dm_names`.
#[repr(C)]
struct DmNames {
    dev: u64,
    next: u32,
    name: [c_char; 0],
}

/// C layout of the fixed prefix of `struct dm_deps`.
#[repr(C)]
struct DmDeps {
    count: u32,
    filler: u32,
    device: [u64; 0],
}

type TaskCreate = unsafe extern "C" fn(c_int) -> *mut TaskRaw;
type TaskDestroy = unsafe extern "C" fn(*mut TaskRaw);
type TaskSetName = unsafe extern "C" fn(*mut TaskRaw, *const c_char) -> c_int;
type TaskRun = unsafe extern "C" fn(*mut TaskRaw) -> c_int;
type TaskGetNames = unsafe extern "C" fn(*mut TaskRaw) -> *mut DmNames;
type TaskGetInfo = unsafe extern "C" fn(*mut TaskRaw, *mut DmInfo) -> c_int;
type TaskGetUuid = unsafe extern "C" fn(*const TaskRaw) -> *const c_char;
type TaskGetDeps = unsafe extern "C" fn(*mut TaskRaw) -> *mut DmDeps;
type GetNextTarget = unsafe extern "C" fn(
    *mut TaskRaw,
    *mut c_void,
    *mut u64,
    *mut u64,
    *mut *mut c_char,
    *mut *mut c_char,
) -> *mut c_void;

/// Symbols used by the libdevmapper adapter, borrowed from its library.
struct Api<'library> {
    task_create: Symbol<'library, TaskCreate>,
    task_destroy: Symbol<'library, TaskDestroy>,
    task_set_name: Symbol<'library, TaskSetName>,
    task_run: Symbol<'library, TaskRun>,
    task_get_names: Symbol<'library, TaskGetNames>,
    task_get_info: Symbol<'library, TaskGetInfo>,
    task_get_uuid: Symbol<'library, TaskGetUuid>,
    task_get_deps: Symbol<'library, TaskGetDeps>,
    get_next_target: Symbol<'library, GetNextTarget>,
}

impl<'library> Api<'library> {
    /// Resolves the exact libdevmapper ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every Rust function type below matches libdevmapper.h.
        // Symbols retain `library`'s lifetime.
        unsafe {
            Ok(Self {
                task_create: load_symbol(library, b"dm_task_create\0")?,
                task_destroy: load_symbol(library, b"dm_task_destroy\0")?,
                task_set_name: load_symbol(library, b"dm_task_set_name\0")?,
                task_run: load_symbol(library, b"dm_task_run\0")?,
                task_get_names: load_symbol(library, b"dm_task_get_names\0")?,
                task_get_info: load_symbol(library, b"dm_task_get_info\0")?,
                task_get_uuid: load_symbol(library, b"dm_task_get_uuid\0")?,
                task_get_deps: load_symbol(library, b"dm_task_get_deps\0")?,
                get_next_target: load_symbol(library, b"dm_get_next_target\0")?,
            })
        }
    }
}

/// Reads active device-mapper topology through a borrowed library handle.
pub(super) fn probe(library: &Library) -> Result<Vec<DevmapperEntry>, NativeProbeError> {
    let api = Api::load(library)?;
    list_names(&api)?
        .into_iter()
        .map(|name| inspect_mapping(&api, &name))
        .collect()
}

/// Returns all live device-mapper names.
fn list_names(api: &Api<'_>) -> Result<Vec<String>, NativeProbeError> {
    let task = Task::run(api, DM_DEVICE_LIST, None)?;
    // SAFETY: task completed successfully and owns the returned list buffer.
    let mut current = unsafe { (api.task_get_names)(task.pointer) };
    let mut names = Vec::new();
    while !current.is_null() {
        if names.len() == MAX_NATIVE_ENTRIES {
            return Err(NativeProbeError::InvalidData {
                operation: "dm_task_get_names",
            });
        }
        // SAFETY: current points into the task-owned list. `name` is the
        // NUL-terminated flexible-array member documented by libdevmapper.
        let name = unsafe {
            CStr::from_ptr(ptr::addr_of!((*current).name).cast::<c_char>())
                .to_string_lossy()
                .into_owned()
        };
        if !name.is_empty() {
            names.push(name);
        }
        // SAFETY: current points to a valid fixed prefix in the task buffer.
        let next = unsafe { (*current).next };
        if next == 0 {
            break;
        }
        if usize::try_from(next)
            .ok()
            .is_none_or(|offset| offset < size_of::<DmNames>())
        {
            return Err(NativeProbeError::InvalidData {
                operation: "dm_task_get_names",
            });
        }
        // SAFETY: libdevmapper defines `next` as a byte offset to the next
        // `dm_names` record within the same task-owned buffer.
        current = unsafe { current.byte_add(next as usize) };
    }
    Ok(names)
}

/// Inspects one mapping table and its backing device numbers.
fn inspect_mapping(api: &Api<'_>, name: &str) -> Result<DevmapperEntry, NativeProbeError> {
    let table = Task::run(api, DM_DEVICE_TABLE, Some(name))?;
    let mut info = DmInfo::default();
    // SAFETY: task completed successfully and output points to the exact C layout.
    if unsafe { (api.task_get_info)(table.pointer, &mut info) } == 0 {
        return Err(call_failed("dm_task_get_info"));
    }
    // SAFETY: UUID is borrowed from the live task and copied immediately.
    let uuid = unsafe { copied_string((api.task_get_uuid)(table.pointer)) };
    let targets = read_targets(api, &table)?;
    let deps = Task::run(api, DM_DEVICE_DEPS, Some(name))?;
    let dependencies = read_dependencies(api, &deps)?;
    Ok(DevmapperEntry {
        name: name.to_owned(),
        uuid,
        devno: NativeDeviceNumber {
            major: info.major,
            minor: info.minor,
        },
        targets,
        dependencies,
        read_only: info.read_only != 0,
    })
}

/// Copies target type names from a completed table task.
fn read_targets(api: &Api<'_>, task: &Task<'_, '_>) -> Result<Vec<String>, NativeProbeError> {
    let mut targets = Vec::new();
    let mut next = std::ptr::null_mut();
    loop {
        let mut start = 0;
        let mut length = 0;
        let mut target_type = std::ptr::null_mut();
        let mut params = std::ptr::null_mut();
        // SAFETY: task is a completed table task and all output pointers live
        // through the call. Returned strings are task-owned.
        next = unsafe {
            (api.get_next_target)(
                task.pointer,
                next,
                &mut start,
                &mut length,
                &mut target_type,
                &mut params,
            )
        };
        // SAFETY: libdevmapper writes the current target before returning the
        // continuation pointer; the final target therefore has `next == null`.
        if let Some(target) = unsafe { copied_string(target_type) }
            && !targets.contains(&target)
        {
            targets.push(target);
        }
        if targets.len() == MAX_NATIVE_ENTRIES {
            return Err(NativeProbeError::InvalidData {
                operation: "dm_get_next_target",
            });
        }
        if next.is_null() {
            break;
        }
    }
    Ok(targets)
}

/// Copies backing device numbers from a completed dependency task.
fn read_dependencies(
    api: &Api<'_>,
    task: &Task<'_, '_>,
) -> Result<Vec<NativeDeviceNumber>, NativeProbeError> {
    // SAFETY: task completed successfully and owns the returned buffer.
    let deps = unsafe { (api.task_get_deps)(task.pointer) };
    if deps.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: deps points to the fixed prefix of the task-owned result.
    let count = unsafe { (*deps).count as usize };
    if count > MAX_NATIVE_ENTRIES {
        return Err(NativeProbeError::InvalidData {
            operation: "dm_task_get_deps",
        });
    }
    // SAFETY: libdevmapper guarantees `count` flexible-array entries.
    let devices =
        unsafe { std::slice::from_raw_parts(ptr::addr_of!((*deps).device).cast::<u64>(), count) };
    Ok(devices.iter().copied().map(decode_devno).collect())
}

/// Owns a completed libdevmapper task.
struct Task<'api, 'library> {
    pointer: *mut TaskRaw,
    api: &'api Api<'library>,
}

impl<'api, 'library> Task<'api, 'library> {
    /// Creates, configures, and executes one read-only task.
    fn run(
        api: &'api Api<'library>,
        task_type: c_int,
        name: Option<&str>,
    ) -> Result<Self, NativeProbeError> {
        // SAFETY: task type is one of libdevmapper's documented query values.
        let pointer = unsafe { (api.task_create)(task_type) };
        if pointer.is_null() {
            return Err(NativeProbeError::AllocationFailed {
                object: "libdevmapper task",
            });
        }
        let task = Self { pointer, api };
        if let Some(name) = name {
            let name = CString::new(name).map_err(|_| NativeProbeError::InvalidData {
                operation: "dm_task_set_name",
            })?;
            // SAFETY: task and NUL-terminated mapping name are live.
            if unsafe { (api.task_set_name)(task.pointer, name.as_ptr()) } == 0 {
                return Err(call_failed("dm_task_set_name"));
            }
        }
        // SAFETY: task is configured for a read-only query operation.
        if unsafe { (api.task_run)(task.pointer) } == 0 {
            return Err(call_failed("dm_task_run"));
        }
        Ok(task)
    }
}

impl Drop for Task<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: this is the single matching destroy for the owned task.
        unsafe { (self.api.task_destroy)(self.pointer) };
    }
}

/// Decodes Linux's `dev_t` representation into major and minor numbers.
fn decode_devno(device: u64) -> NativeDeviceNumber {
    NativeDeviceNumber {
        major: (((device >> 8) & 0x0fff) | ((device >> 32) & 0xffff_f000)) as u32,
        minor: ((device & 0x00ff) | ((device >> 12) & 0xffff_ff00)) as u32,
    }
}

/// Creates the shared error for libdevmapper's boolean failure convention.
fn call_failed(operation: &'static str) -> NativeProbeError {
    NativeProbeError::CallFailed { operation, code: 0 }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies Linux `dev_t` decoding for ordinary and expanded values.
    #[test]
    fn decodes_linux_device_numbers() {
        assert_eq!(
            decode_devno((259_u64 << 8) | 3),
            NativeDeviceNumber {
                major: 259,
                minor: 3
            }
        );
    }
}
