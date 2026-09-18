//! Typed libfdisk FFI adapter.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_ulong},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{FdiskPartition, FdiskTable, NativeProbeError};

/// Opaque libfdisk context.
enum ContextRaw {}
/// Opaque libfdisk label.
enum LabelRaw {}
/// Opaque libfdisk table.
enum TableRaw {}
/// Opaque libfdisk partition.
enum PartitionRaw {}
/// Opaque libfdisk partition type.
enum PartTypeRaw {}

type NewContext = unsafe extern "C" fn() -> *mut ContextRaw;
type UnrefContext = unsafe extern "C" fn(*mut ContextRaw);
type AssignDevice = unsafe extern "C" fn(*mut ContextRaw, *const c_char, c_int) -> c_int;
type DeassignDevice = unsafe extern "C" fn(*mut ContextRaw, c_int) -> c_int;
type GetLabel = unsafe extern "C" fn(*mut ContextRaw, *const c_char) -> *mut LabelRaw;
type LabelName = unsafe extern "C" fn(*const LabelRaw) -> *const c_char;
type SectorSize = unsafe extern "C" fn(*mut ContextRaw) -> c_ulong;
type GetPartitions = unsafe extern "C" fn(*mut ContextRaw, *mut *mut TableRaw) -> c_int;
type UnrefTable = unsafe extern "C" fn(*mut TableRaw);
type TableLen = unsafe extern "C" fn(*mut TableRaw) -> usize;
type TablePartition = unsafe extern "C" fn(*mut TableRaw, usize) -> *mut PartitionRaw;
type GetSector = unsafe extern "C" fn(*mut PartitionRaw) -> u64;
type GetPartno = unsafe extern "C" fn(*mut PartitionRaw) -> usize;
type GetText = unsafe extern "C" fn(*mut PartitionRaw) -> *const c_char;
type GetType = unsafe extern "C" fn(*mut PartitionRaw) -> *mut PartTypeRaw;
type TypeText = unsafe extern "C" fn(*const PartTypeRaw) -> *const c_char;
type IsBootable = unsafe extern "C" fn(*mut PartitionRaw) -> c_int;

/// Symbols used by the libfdisk adapter, borrowed from its loaded library.
struct Api<'library> {
    new_context: Symbol<'library, NewContext>,
    unref_context: Symbol<'library, UnrefContext>,
    assign_device: Symbol<'library, AssignDevice>,
    deassign_device: Symbol<'library, DeassignDevice>,
    get_label: Symbol<'library, GetLabel>,
    label_name: Symbol<'library, LabelName>,
    sector_size: Symbol<'library, SectorSize>,
    get_partitions: Symbol<'library, GetPartitions>,
    unref_table: Symbol<'library, UnrefTable>,
    table_len: Symbol<'library, TableLen>,
    table_partition: Symbol<'library, TablePartition>,
    get_start: Symbol<'library, GetSector>,
    get_size: Symbol<'library, GetSector>,
    get_partno: Symbol<'library, GetPartno>,
    get_name: Symbol<'library, GetText>,
    get_uuid: Symbol<'library, GetText>,
    get_type: Symbol<'library, GetType>,
    type_text: Symbol<'library, TypeText>,
    is_bootable: Symbol<'library, IsBootable>,
}

impl<'library> Api<'library> {
    /// Resolves the exact libfdisk ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every Rust function type below matches libfdisk.h. Symbols
        // retain `library`'s lifetime.
        unsafe {
            Ok(Self {
                new_context: load_symbol(library, b"fdisk_new_context\0")?,
                unref_context: load_symbol(library, b"fdisk_unref_context\0")?,
                assign_device: load_symbol(library, b"fdisk_assign_device\0")?,
                deassign_device: load_symbol(library, b"fdisk_deassign_device\0")?,
                get_label: load_symbol(library, b"fdisk_get_label\0")?,
                label_name: load_symbol(library, b"fdisk_label_get_name\0")?,
                sector_size: load_symbol(library, b"fdisk_get_sector_size\0")?,
                get_partitions: load_symbol(library, b"fdisk_get_partitions\0")?,
                unref_table: load_symbol(library, b"fdisk_unref_table\0")?,
                table_len: load_symbol(library, b"fdisk_table_get_nents\0")?,
                table_partition: load_symbol(library, b"fdisk_table_get_partition\0")?,
                get_start: load_symbol(library, b"fdisk_partition_get_start\0")?,
                get_size: load_symbol(library, b"fdisk_partition_get_size\0")?,
                get_partno: load_symbol(library, b"fdisk_partition_get_partno\0")?,
                get_name: load_symbol(library, b"fdisk_partition_get_name\0")?,
                get_uuid: load_symbol(library, b"fdisk_partition_get_uuid\0")?,
                get_type: load_symbol(library, b"fdisk_partition_get_type\0")?,
                type_text: load_symbol(library, b"fdisk_parttype_get_string\0")?,
                is_bootable: load_symbol(library, b"fdisk_partition_is_bootable\0")?,
            })
        }
    }
}

/// Reads partition tables for the supplied whole-disk paths.
pub(super) fn probe(
    library: &Library,
    devices: &[PathBuf],
) -> Result<Vec<FdiskTable>, NativeProbeError> {
    let api = Api::load(library)?;
    devices
        .iter()
        .filter_map(|device| match probe_device(&api, device) {
            Ok(Some(table)) => Some(Ok(table)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

/// Reads one device, returning `None` when libfdisk cannot open it read-only.
fn probe_device(api: &Api<'_>, device: &Path) -> Result<Option<FdiskTable>, NativeProbeError> {
    let path =
        CString::new(device.as_os_str().as_bytes()).map_err(|_| NativeProbeError::InvalidPath {
            path: device.to_path_buf(),
        })?;
    // SAFETY: constructor takes no arguments and returns an owned context.
    let pointer = unsafe { (api.new_context)() };
    if pointer.is_null() {
        return Err(NativeProbeError::AllocationFailed {
            object: "libfdisk context",
        });
    }
    let mut context = Context {
        pointer,
        api,
        assigned: false,
    };
    // SAFETY: path is NUL-terminated; `1` requests read-only access.
    if unsafe { (api.assign_device)(context.pointer, path.as_ptr(), 1) } != 0 {
        return Ok(None);
    }
    context.assigned = true;

    // SAFETY: context is assigned; null requests the current label.
    let label = unsafe { (api.get_label)(context.pointer, std::ptr::null()) };
    let label = if label.is_null() {
        None
    } else {
        // SAFETY: label belongs to the assigned live context.
        unsafe { copied_string((api.label_name)(label)) }
    };
    let mut table_pointer = std::ptr::null_mut();
    // SAFETY: context and output pointer are live.
    let result = unsafe { (api.get_partitions)(context.pointer, &mut table_pointer) };
    let partitions = if result == 0 && !table_pointer.is_null() {
        read_partitions(api, table_pointer)
    } else {
        Vec::new()
    };
    // SAFETY: assigned context is live.
    let sector_size = unsafe { (api.sector_size)(context.pointer) };
    Ok(Some(FdiskTable {
        device: device.to_path_buf(),
        label,
        sector_size,
        partitions,
    }))
}

/// Copies every partition from one owned libfdisk table.
fn read_partitions(api: &Api<'_>, pointer: *mut TableRaw) -> Vec<FdiskPartition> {
    let table = Table { pointer, api };
    // SAFETY: table is live.
    let length = unsafe { (api.table_len)(table.pointer) };
    let mut partitions = Vec::with_capacity(length);
    for index in 0..length {
        // SAFETY: index is within the table's reported length.
        let partition = unsafe { (api.table_partition)(table.pointer, index) };
        if !partition.is_null() {
            partitions.push(read_partition(api, partition));
        }
    }
    partitions
}

/// Copies one partition borrowed from a live libfdisk table.
fn read_partition(api: &Api<'_>, partition: *mut PartitionRaw) -> FdiskPartition {
    // SAFETY: partition is borrowed from a live table; returned strings are
    // copied before the table can be released.
    unsafe {
        let part_type = (api.get_type)(partition);
        FdiskPartition {
            number: u32::try_from((api.get_partno)(partition).saturating_add(1))
                .unwrap_or(u32::MAX),
            start_sectors: (api.get_start)(partition),
            size_sectors: (api.get_size)(partition),
            partition_type: (!part_type.is_null())
                .then(|| copied_string((api.type_text)(part_type)))
                .flatten(),
            name: copied_string((api.get_name)(partition)),
            uuid: copied_string((api.get_uuid)(partition)),
            bootable: (api.is_bootable)(partition) == 1,
        }
    }
}

/// Owns one libfdisk context and its optional device assignment.
struct Context<'api, 'library> {
    pointer: *mut ContextRaw,
    api: &'api Api<'library>,
    assigned: bool,
}

impl Drop for Context<'_, '_> {
    fn drop(&mut self) {
        if self.assigned {
            // SAFETY: context has one active read-only assignment.
            let _ = unsafe { (self.api.deassign_device)(self.pointer, 0) };
        }
        // SAFETY: this is the single matching unref for the owned context.
        unsafe { (self.api.unref_context)(self.pointer) };
    }
}

/// Owns one libfdisk partition table.
struct Table<'api, 'library> {
    pointer: *mut TableRaw,
    api: &'api Api<'library>,
}

impl Drop for Table<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: this is the single matching unref for the owned table.
        unsafe { (self.api.unref_table)(self.pointer) };
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
