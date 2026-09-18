//! Typed libudev FFI adapter.

use std::{
    ffi::{CStr, OsString, c_char, c_int},
    os::unix::ffi::OsStringExt,
    path::PathBuf,
};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{NativeProbeError, UdevBlockEntry};

/// Opaque libudev context.
enum UdevRaw {}
/// Opaque libudev enumerator.
enum EnumerateRaw {}
/// Opaque libudev list item.
enum ListEntryRaw {}
/// Opaque libudev device.
enum DeviceRaw {}

type NewUdev = unsafe extern "C" fn() -> *mut UdevRaw;
type UnrefUdev = unsafe extern "C" fn(*mut UdevRaw) -> *mut UdevRaw;
type NewEnumerate = unsafe extern "C" fn(*mut UdevRaw) -> *mut EnumerateRaw;
type UnrefEnumerate = unsafe extern "C" fn(*mut EnumerateRaw) -> *mut EnumerateRaw;
type MatchSubsystem = unsafe extern "C" fn(*mut EnumerateRaw, *const c_char) -> c_int;
type ScanDevices = unsafe extern "C" fn(*mut EnumerateRaw) -> c_int;
type GetList = unsafe extern "C" fn(*mut EnumerateRaw) -> *mut ListEntryRaw;
type GetNext = unsafe extern "C" fn(*mut ListEntryRaw) -> *mut ListEntryRaw;
type GetName = unsafe extern "C" fn(*mut ListEntryRaw) -> *const c_char;
type NewDevice = unsafe extern "C" fn(*mut UdevRaw, *const c_char) -> *mut DeviceRaw;
type UnrefDevice = unsafe extern "C" fn(*mut DeviceRaw) -> *mut DeviceRaw;
type DeviceGetter = unsafe extern "C" fn(*mut DeviceRaw) -> *const c_char;
type DeviceKeyGetter = unsafe extern "C" fn(*mut DeviceRaw, *const c_char) -> *const c_char;

/// Symbols used by the libudev adapter, borrowed from its loaded library.
struct Api<'library> {
    new_udev: Symbol<'library, NewUdev>,
    unref_udev: Symbol<'library, UnrefUdev>,
    new_enumerate: Symbol<'library, NewEnumerate>,
    unref_enumerate: Symbol<'library, UnrefEnumerate>,
    match_subsystem: Symbol<'library, MatchSubsystem>,
    scan_devices: Symbol<'library, ScanDevices>,
    get_list: Symbol<'library, GetList>,
    get_next: Symbol<'library, GetNext>,
    get_name: Symbol<'library, GetName>,
    new_device: Symbol<'library, NewDevice>,
    unref_device: Symbol<'library, UnrefDevice>,
    get_devnode: Symbol<'library, DeviceGetter>,
    get_sysname: Symbol<'library, DeviceGetter>,
    get_devtype: Symbol<'library, DeviceGetter>,
    get_property: Symbol<'library, DeviceKeyGetter>,
    get_sysattr: Symbol<'library, DeviceKeyGetter>,
}

impl<'library> Api<'library> {
    /// Resolves the exact libudev ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every Rust function type below matches libudev.h. Symbols
        // retain `library`'s lifetime.
        unsafe {
            Ok(Self {
                new_udev: load_symbol(library, b"udev_new\0")?,
                unref_udev: load_symbol(library, b"udev_unref\0")?,
                new_enumerate: load_symbol(library, b"udev_enumerate_new\0")?,
                unref_enumerate: load_symbol(library, b"udev_enumerate_unref\0")?,
                match_subsystem: load_symbol(library, b"udev_enumerate_add_match_subsystem\0")?,
                scan_devices: load_symbol(library, b"udev_enumerate_scan_devices\0")?,
                get_list: load_symbol(library, b"udev_enumerate_get_list_entry\0")?,
                get_next: load_symbol(library, b"udev_list_entry_get_next\0")?,
                get_name: load_symbol(library, b"udev_list_entry_get_name\0")?,
                new_device: load_symbol(library, b"udev_device_new_from_syspath\0")?,
                unref_device: load_symbol(library, b"udev_device_unref\0")?,
                get_devnode: load_symbol(library, b"udev_device_get_devnode\0")?,
                get_sysname: load_symbol(library, b"udev_device_get_sysname\0")?,
                get_devtype: load_symbol(library, b"udev_device_get_devtype\0")?,
                get_property: load_symbol(library, b"udev_device_get_property_value\0")?,
                get_sysattr: load_symbol(library, b"udev_device_get_sysattr_value\0")?,
            })
        }
    }
}

/// Enumerates block endpoints through a borrowed libudev handle.
pub(super) fn probe(library: &Library) -> Result<Vec<UdevBlockEntry>, NativeProbeError> {
    let api = Api::load(library)?;
    // SAFETY: constructor has no arguments and returns an owned reference.
    let pointer = unsafe { (api.new_udev)() };
    if pointer.is_null() {
        return Err(NativeProbeError::AllocationFailed {
            object: "udev context",
        });
    }
    let udev = Udev { pointer, api: &api };

    // SAFETY: the context is live; the result is an owned reference.
    let pointer = unsafe { (api.new_enumerate)(udev.pointer) };
    if pointer.is_null() {
        return Err(NativeProbeError::AllocationFailed {
            object: "udev enumerator",
        });
    }
    let enumerate = Enumerate { pointer, api: &api };
    // SAFETY: enumerator is live and the static string is NUL-terminated.
    check_zero("udev_enumerate_add_match_subsystem", unsafe {
        (api.match_subsystem)(enumerate.pointer, c"block".as_ptr())
    })?;
    // SAFETY: enumerator is initialized and live.
    check_zero("udev_enumerate_scan_devices", unsafe {
        (api.scan_devices)(enumerate.pointer)
    })?;

    let mut entries = Vec::new();
    // SAFETY: list is borrowed from the live enumerator.
    let mut item = unsafe { (api.get_list)(enumerate.pointer) };
    while !item.is_null() {
        // SAFETY: list item belongs to the live enumerator.
        let syspath = unsafe { (api.get_name)(item) };
        if !syspath.is_null() {
            // SAFETY: context and borrowed syspath are live.
            let pointer = unsafe { (api.new_device)(udev.pointer, syspath) };
            if !pointer.is_null() {
                let device = Device { pointer, api: &api };
                // SAFETY: getters return device-owned C strings copied before
                // the device guard is dropped.
                if let (Some(devnode), Some(sysname)) = unsafe {
                    (
                        copied_bytes((api.get_devnode)(device.pointer)),
                        copied_string((api.get_sysname)(device.pointer)),
                    )
                } {
                    entries.push(entry(&api, &device, devnode, sysname));
                }
            }
        }
        // SAFETY: item belongs to the live enumeration list.
        item = unsafe { (api.get_next)(item) };
    }
    Ok(entries)
}

/// Copies all relevant properties from one live libudev device.
fn entry(
    api: &Api<'_>,
    device: &Device<'_, '_>,
    devnode: Vec<u8>,
    sysname: String,
) -> UdevBlockEntry {
    let property = |key: &'static CStr| {
        // SAFETY: device and key are live; the returned string is copied.
        unsafe { copied_string((api.get_property)(device.pointer, key.as_ptr())) }
    };
    let sysattr = |key: &'static CStr| {
        // SAFETY: device and key are live; the returned string is copied.
        unsafe { copied_string((api.get_sysattr)(device.pointer, key.as_ptr())) }
    };
    UdevBlockEntry {
        devnode: PathBuf::from(OsString::from_vec(devnode)),
        sysname,
        // SAFETY: device remains live and the returned string is copied.
        devtype: unsafe { copied_string((api.get_devtype)(device.pointer)) },
        major: property(c"MAJOR")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        minor: property(c"MINOR")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        size_sectors: sysattr(c"size").and_then(|value| value.parse().ok()),
        logical_block_size: sysattr(c"queue/logical_block_size")
            .and_then(|value| value.parse().ok()),
        physical_block_size: sysattr(c"queue/physical_block_size")
            .and_then(|value| value.parse().ok()),
        model: property(c"ID_MODEL").map(|value| value.trim().to_owned()),
        serial: property(c"ID_SERIAL_SHORT"),
        wwn: property(c"ID_WWN"),
        read_only: sysattr(c"ro").and_then(parse_bool),
        rotational: sysattr(c"queue/rotational").and_then(parse_bool),
        removable: sysattr(c"removable").and_then(parse_bool),
    }
}

/// Owns one libudev context reference.
struct Udev<'api, 'library> {
    pointer: *mut UdevRaw,
    api: &'api Api<'library>,
}

impl Drop for Udev<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: this is the single matching unref for the owned reference.
        let _ = unsafe { (self.api.unref_udev)(self.pointer) };
    }
}

/// Owns one libudev enumerator reference.
struct Enumerate<'api, 'library> {
    pointer: *mut EnumerateRaw,
    api: &'api Api<'library>,
}

impl Drop for Enumerate<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: this is the single matching unref for the owned reference.
        let _ = unsafe { (self.api.unref_enumerate)(self.pointer) };
    }
}

/// Owns one libudev device reference.
struct Device<'api, 'library> {
    pointer: *mut DeviceRaw,
    api: &'api Api<'library>,
}

impl Drop for Device<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: this is the single matching unref for the owned reference.
        let _ = unsafe { (self.api.unref_device)(self.pointer) };
    }
}

/// Converts libudev's zero-success result into the shared error.
fn check_zero(operation: &'static str, code: c_int) -> Result<(), NativeProbeError> {
    if code == 0 {
        Ok(())
    } else {
        Err(NativeProbeError::CallFailed { operation, code })
    }
}

/// Parses kernel boolean attributes represented by `0` and `1`.
fn parse_bool(value: String) -> Option<bool> {
    match value.as_str() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
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
