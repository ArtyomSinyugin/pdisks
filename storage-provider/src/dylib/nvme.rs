//! Typed read-only libnvme FFI adapter.

use std::ffi::{CStr, c_char, c_int};

use libloading::{Library, Symbol};

use super::load_symbol;
use crate::{
    NativeProbeError, NvmeControllerEntry, NvmeEntry, NvmeNamespaceEntry, NvmeSubsystemEntry,
};

const MAX_NATIVE_ENTRIES: usize = 65_536;

/// Opaque libnvme root.
enum RootRaw {}
/// Opaque libnvme host.
enum HostRaw {}
/// Opaque libnvme subsystem.
enum SubsystemRaw {}
/// Opaque libnvme controller.
enum ControllerRaw {}
/// Opaque libnvme namespace.
enum NamespaceRaw {}

type Scan = unsafe extern "C" fn(*const c_char) -> *mut RootRaw;
type FreeTree = unsafe extern "C" fn(*mut RootRaw);
type FirstHost = unsafe extern "C" fn(*mut RootRaw) -> *mut HostRaw;
type NextHost = unsafe extern "C" fn(*mut RootRaw, *mut HostRaw) -> *mut HostRaw;
type FirstSubsystem = unsafe extern "C" fn(*mut HostRaw) -> *mut SubsystemRaw;
type NextSubsystem = unsafe extern "C" fn(*mut HostRaw, *mut SubsystemRaw) -> *mut SubsystemRaw;
type FirstController = unsafe extern "C" fn(*mut SubsystemRaw) -> *mut ControllerRaw;
type NextController =
    unsafe extern "C" fn(*mut SubsystemRaw, *mut ControllerRaw) -> *mut ControllerRaw;
type FirstNamespace = unsafe extern "C" fn(*mut SubsystemRaw) -> *mut NamespaceRaw;
type NextNamespace =
    unsafe extern "C" fn(*mut SubsystemRaw, *mut NamespaceRaw) -> *mut NamespaceRaw;
type GetText<T> = unsafe extern "C" fn(*mut T) -> *const c_char;
type GetInt = unsafe extern "C" fn(*mut NamespaceRaw) -> c_int;
type GetBytes = unsafe extern "C" fn(*mut NamespaceRaw) -> *const u8;
type GetUuid = unsafe extern "C" fn(*mut NamespaceRaw, *mut u8);

/// Symbols used by the libnvme topology adapter.
struct Api<'library> {
    scan: Symbol<'library, Scan>,
    free_tree: Symbol<'library, FreeTree>,
    first_host: Symbol<'library, FirstHost>,
    next_host: Symbol<'library, NextHost>,
    first_subsystem: Symbol<'library, FirstSubsystem>,
    next_subsystem: Symbol<'library, NextSubsystem>,
    first_controller: Symbol<'library, FirstController>,
    next_controller: Symbol<'library, NextController>,
    subsystem_name: Symbol<'library, GetText<SubsystemRaw>>,
    subsystem_nqn: Symbol<'library, GetText<SubsystemRaw>>,
    controller_name: Symbol<'library, GetText<ControllerRaw>>,
    controller_transport: Symbol<'library, GetText<ControllerRaw>>,
    first_namespace: Symbol<'library, FirstNamespace>,
    next_namespace: Symbol<'library, NextNamespace>,
    namespace_name: Symbol<'library, GetText<NamespaceRaw>>,
    namespace_nsid: Symbol<'library, GetInt>,
    namespace_lba_size: Symbol<'library, GetInt>,
    namespace_meta_size: Symbol<'library, GetInt>,
    namespace_nguid: Symbol<'library, GetBytes>,
    namespace_eui64: Symbol<'library, GetBytes>,
    namespace_uuid: Symbol<'library, GetUuid>,
}

impl<'library> Api<'library> {
    /// Resolves the exact libnvme tree ABI used by this adapter.
    fn load(library: &'library Library) -> Result<Self, NativeProbeError> {
        // SAFETY: every function type matches nvme/tree.h and symbols retain
        // the library lifetime.
        unsafe {
            Ok(Self {
                scan: load_symbol(library, b"nvme_scan\0")?,
                free_tree: load_symbol(library, b"nvme_free_tree\0")?,
                first_host: load_symbol(library, b"nvme_first_host\0")?,
                next_host: load_symbol(library, b"nvme_next_host\0")?,
                first_subsystem: load_symbol(library, b"nvme_first_subsystem\0")?,
                next_subsystem: load_symbol(library, b"nvme_next_subsystem\0")?,
                first_controller: load_symbol(library, b"nvme_subsystem_first_ctrl\0")?,
                next_controller: load_symbol(library, b"nvme_subsystem_next_ctrl\0")?,
                subsystem_name: load_symbol(library, b"nvme_subsystem_get_name\0")?,
                subsystem_nqn: load_symbol(library, b"nvme_subsystem_get_nqn\0")?,
                controller_name: load_symbol(library, b"nvme_ctrl_get_name\0")?,
                controller_transport: load_symbol(library, b"nvme_ctrl_get_transport\0")?,
                first_namespace: load_symbol(library, b"nvme_subsystem_first_ns\0")?,
                next_namespace: load_symbol(library, b"nvme_subsystem_next_ns\0")?,
                namespace_name: load_symbol(library, b"nvme_ns_get_name\0")?,
                namespace_nsid: load_symbol(library, b"nvme_ns_get_nsid\0")?,
                namespace_lba_size: load_symbol(library, b"nvme_ns_get_lba_size\0")?,
                namespace_meta_size: load_symbol(library, b"nvme_ns_get_meta_size\0")?,
                namespace_nguid: load_symbol(library, b"nvme_ns_get_nguid\0")?,
                namespace_eui64: load_symbol(library, b"nvme_ns_get_eui64\0")?,
                namespace_uuid: load_symbol(library, b"nvme_ns_get_uuid\0")?,
            })
        }
    }
}

/// Scans and copies the live NVMe tree.
pub(super) fn probe(library: &Library) -> Result<NvmeEntry, NativeProbeError> {
    let api = Api::load(library)?;
    // SAFETY: a null config path requests the live topology with defaults.
    let pointer = unsafe { (api.scan)(std::ptr::null()) };
    if pointer.is_null() {
        return Err(NativeProbeError::AllocationFailed {
            object: "libnvme topology",
        });
    }
    let root = RootGuard { pointer, api: &api };
    let mut subsystems = Vec::new();
    // SAFETY: root remains live throughout traversal.
    let mut host = unsafe { (api.first_host)(root.pointer) };
    while !host.is_null() {
        // SAFETY: host belongs to the live root.
        let mut subsystem = unsafe { (api.first_subsystem)(host) };
        while !subsystem.is_null() {
            subsystems.push(inspect_subsystem(&api, subsystem)?);
            if subsystems.len() == MAX_NATIVE_ENTRIES {
                return Err(invalid("nvme subsystem traversal"));
            }
            // SAFETY: both host and current subsystem belong to the live root.
            subsystem = unsafe { (api.next_subsystem)(host, subsystem) };
        }
        // SAFETY: root and current host remain live.
        host = unsafe { (api.next_host)(root.pointer, host) };
    }
    Ok(NvmeEntry { subsystems })
}

/// Copies one subsystem and its immediate children.
fn inspect_subsystem(
    api: &Api<'_>,
    subsystem: *mut SubsystemRaw,
) -> Result<NvmeSubsystemEntry, NativeProbeError> {
    // SAFETY: strings are borrowed from the live subsystem and copied now.
    let (name, nqn) = unsafe {
        (
            copied_string((api.subsystem_name)(subsystem)),
            copied_string((api.subsystem_nqn)(subsystem)),
        )
    };
    let (Some(name), Some(nqn)) = (name, nqn) else {
        return Err(invalid("nvme subsystem identity"));
    };
    let mut controllers = Vec::new();
    // SAFETY: subsystem remains live during traversal.
    let mut controller = unsafe { (api.first_controller)(subsystem) };
    while !controller.is_null() {
        // SAFETY: controller strings are borrowed and copied immediately.
        let controller_name = unsafe { copied_string((api.controller_name)(controller)) };
        if let Some(name) = controller_name {
            // SAFETY: transport is an optional borrowed controller string.
            let transport = unsafe { copied_string((api.controller_transport)(controller)) };
            controllers.push(NvmeControllerEntry { name, transport });
        }
        if controllers.len() == MAX_NATIVE_ENTRIES {
            return Err(invalid("nvme controller traversal"));
        }
        // SAFETY: subsystem and current controller remain live.
        controller = unsafe { (api.next_controller)(subsystem, controller) };
    }

    let mut namespaces = Vec::new();
    // SAFETY: subsystem remains live during traversal.
    let mut namespace = unsafe { (api.first_namespace)(subsystem) };
    while !namespace.is_null() {
        if let Some(entry) = inspect_namespace(api, namespace)? {
            namespaces.push(entry);
        }
        if namespaces.len() == MAX_NATIVE_ENTRIES {
            return Err(invalid("nvme namespace traversal"));
        }
        // SAFETY: subsystem and current namespace remain live.
        namespace = unsafe { (api.next_namespace)(subsystem, namespace) };
    }
    Ok(NvmeSubsystemEntry {
        name,
        nqn,
        controllers,
        namespaces,
    })
}

/// Copies one namespace when its mandatory model fields are valid.
fn inspect_namespace(
    api: &Api<'_>,
    namespace: *mut NamespaceRaw,
) -> Result<Option<NvmeNamespaceEntry>, NativeProbeError> {
    // SAFETY: all values are borrowed from the live namespace or copied into
    // caller-owned buffers.
    let name = unsafe { copied_string((api.namespace_name)(namespace)) };
    let Some(name) = name else {
        return Ok(None);
    };
    // SAFETY: scalar getters only inspect the live namespace.
    let (nsid, lba_size, metadata_size) = unsafe {
        (
            (api.namespace_nsid)(namespace),
            (api.namespace_lba_size)(namespace),
            (api.namespace_meta_size)(namespace),
        )
    };
    if nsid <= 0 || lba_size <= 0 || !(0..=u16::MAX as c_int).contains(&metadata_size) {
        return Ok(None);
    }
    // SAFETY: identifier pointers, when non-null, reference fixed-size arrays
    // owned by the live namespace.
    let nguid = unsafe { copied_array::<16>((api.namespace_nguid)(namespace)) };
    // SAFETY: same invariant as NGUID, with an eight-byte array.
    let eui64 = unsafe { copied_array::<8>((api.namespace_eui64)(namespace)) };
    let mut uuid = [0_u8; 16];
    // SAFETY: output points to a writable 16-byte buffer.
    unsafe { (api.namespace_uuid)(namespace, uuid.as_mut_ptr()) };
    Ok(Some(NvmeNamespaceEntry {
        name,
        nsid: nsid as u32,
        lba_size: lba_size as u32,
        metadata_size: metadata_size as u16,
        nguid,
        eui64,
        uuid: (uuid != [0; 16]).then_some(uuid),
    }))
}

/// Owns one libnvme root tree.
struct RootGuard<'api, 'library> {
    pointer: *mut RootRaw,
    api: &'api Api<'library>,
}

impl Drop for RootGuard<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: balances the single successful nvme_scan allocation.
        unsafe { (self.api.free_tree)(self.pointer) };
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

/// Copies a non-zero fixed-size identifier from a native pointer.
unsafe fn copied_array<const N: usize>(pointer: *const u8) -> Option<[u8; N]> {
    if pointer.is_null() {
        return None;
    }
    let mut value = [0_u8; N];
    // SAFETY: caller guarantees that pointer references at least N bytes.
    unsafe { std::ptr::copy_nonoverlapping(pointer, value.as_mut_ptr(), N) };
    (value != [0; N]).then_some(value)
}

/// Creates a malformed-native-data error.
fn invalid(operation: &'static str) -> NativeProbeError {
    NativeProbeError::InvalidData { operation }
}
