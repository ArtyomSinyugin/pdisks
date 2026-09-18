#![allow(clippy::expect_used)]
//! Opt-in compatibility report for installed dynamic-library providers.

use std::{collections::BTreeMap, env, path::PathBuf};

use storage_core::model::{MountContext, MountSource, NodeKind};
use storage_probe::{
    LibmountProvider, NativeBlockProvider, NativeLocalProvider, NativeMapperProvider,
    NativeSystemProvider, StateProvider, assemble,
};
use storage_provider::{ProviderBackend, ProviderRegistry, RegisteredProvider};

/// One required native integration and the canonical model area it serves.
struct ExpectedProvider {
    id: &'static str,
    responsibility: &'static str,
    model_contract: &'static str,
    live_state_adapter: bool,
}

/// Native libraries required by the currently planned local-storage model.
const EXPECTED_PROVIDERS: &[ExpectedProvider] = &[
    ExpectedProvider {
        id: "udev-block",
        responsibility: "block.endpoints",
        model_contract: "Disk/NvmeNamespace + BlockFacts + DeviceInfo",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "blkid-signatures",
        responsibility: "content.signatures",
        model_contract: "Filesystem/LuksContainer/Swap + ExternalId",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "fdisk-partitions",
        responsibility: "partition.topology",
        model_contract: "PartitionTable/Partition + Contains",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "libmount-mounts",
        responsibility: "mounts.runtime",
        model_contract: "MountState + ObservedMount",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "devmapper-topology",
        responsibility: "device-mapper.topology",
        model_contract: "DmCryptMapping/DmCache/DmIntegrity/DmVerity",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "cryptsetup-luks",
        responsibility: "luks.metadata",
        model_contract: "LuksContainer + LuksVersion + LuksUuid",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "lvm",
        responsibility: "lvm.topology",
        model_contract: "LvmPv/LvmVg/LvmLv + membership edges",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "btrfs",
        responsibility: "btrfs.topology",
        model_contract: "BtrfsSubvolume + filesystem Provides edges",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "zfs",
        responsibility: "zfs.topology",
        model_contract: "ZfsPool/ZfsDataset/ZfsVolume",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "nvme",
        responsibility: "nvme.topology",
        model_contract: "NvmeSubsystem/Controller/Namespace + native IDs",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "libblockdev",
        responsibility: "storage.operations",
        model_contract: "provider validation/planning/execution capabilities",
        live_state_adapter: false,
    },
    ExpectedProvider {
        id: "mdraid",
        responsibility: "mdraid.topology",
        model_contract: "MdArray/MdMember + MemberOf",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "multipath",
        responsibility: "multipath.topology",
        model_contract: "DmMultipath + Path dependencies",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "loop",
        responsibility: "loop.devices",
        model_contract: "Loop + backing_file + BlockFacts",
        live_state_adapter: true,
    },
    ExpectedProvider {
        id: "swap",
        responsibility: "swap.runtime",
        model_contract: "Swap + activation state diagnostics",
        live_state_adapter: true,
    },
];

/// Returns the real provider manifest selected by the test environment.
fn manifest_path() -> PathBuf {
    env::var_os("PDISKS_PROVIDER_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("PDISKS_PROVIDER_MANIFEST must name the installed manifest"))
}

/// Loads the real provider registry selected by the test environment.
fn registry() -> ProviderRegistry {
    ProviderRegistry::load_from(manifest_path())
        .unwrap_or_else(|error| panic!("load installed provider manifest: {error}"))
}

/// Finds one provider by its stable manifest ID.
fn provider<'a>(registry: &'a ProviderRegistry, id: &str) -> &'a RegisteredProvider {
    registry
        .providers()
        .find(|provider| provider.manifest().id.as_str() == id)
        .unwrap_or_else(|| panic!("required native provider {id} is absent"))
}

#[test]
#[ignore = "requires PDISKS_PROVIDER_MANIFEST and installed native provider libraries"]
/// Resolves every required `.so` and prints its model responsibility.
fn installed_native_provider_abi_matrix_matches_manifest() {
    let path = manifest_path();
    ProviderRegistry::validate_dependencies(&path)
        .unwrap_or_else(|error| panic!("native dependency validation failed: {error}"));
    let registry = registry();

    println!("\nNative provider ABI/model matrix");
    println!("manifest: {}", path.display());
    println!(
        "{:<22} {:<9} {:<28} {:<8} {}",
        "provider", "backend", "responsibility", "symbols", "model contract"
    );
    println!("{}", "-".repeat(120));

    for expected in EXPECTED_PROVIDERS {
        let registered = provider(&registry, expected.id);
        let library = registered
            .manifest()
            .library
            .as_ref()
            .unwrap_or_else(|| panic!("{} has no native library", expected.id));
        assert_eq!(registered.backend(), ProviderBackend::Library);
        println!(
            "{:<22} {:<9} {:<28} {:<8} {}",
            expected.id,
            "library",
            expected.responsibility,
            library.required_symbols.len(),
            expected.model_contract,
        );
        println!("  library: {}", library.path.display());
    }
    println!("\nresolved native providers: {}", EXPECTED_PROVIDERS.len());
}

#[test]
#[ignore = "requires PDISKS_PROVIDER_MANIFEST and installed native provider libraries"]
/// Requires every native integration to feed live data into CurrentState.
fn native_current_state_adapter_matrix_is_complete() {
    let registry = registry();
    let state_providers = EXPECTED_PROVIDERS
        .iter()
        .filter(|provider| provider.responsibility != "storage.operations")
        .collect::<Vec<_>>();
    let implemented = state_providers
        .iter()
        .filter(|provider| provider.live_state_adapter)
        .count();
    let missing = state_providers
        .iter()
        .filter(|provider| !provider.live_state_adapter)
        .map(|provider| provider.id)
        .collect::<Vec<_>>();

    println!("\nNative provider → CurrentState coverage");
    for expected in EXPECTED_PROVIDERS {
        let registered = provider(&registry, expected.id);
        println!(
            "{:<22} backend={:?} responsibility={:<28} adapter={}",
            expected.id,
            registered.backend(),
            expected.responsibility,
            if expected.responsibility == "storage.operations" {
                "N/A"
            } else if expected.live_state_adapter {
                "LIVE"
            } else {
                "ABI ONLY"
            }
        );
    }
    println!(
        "\nlive CurrentState adapters: {implemented}/{}",
        state_providers.len()
    );

    assert!(
        missing.is_empty(),
        "native providers without a live CurrentState adapter: {}",
        missing.join(", ")
    );
}

#[test]
#[ignore = "requires PDISKS_PROVIDER_MANIFEST and installed libmount"]
/// Runs libmount through FFI and prints the resulting CurrentState mount summary.
fn native_libmount_populates_current_state() {
    let registry = registry();
    let descriptor = provider(&registry, "libmount-mounts");
    let adapter = LibmountProvider::new(descriptor);
    let current = assemble::probe(&[&adapter as &dyn StateProvider]);

    let mut sources = BTreeMap::<&'static str, usize>::new();
    for mount in &current.mounts.entries {
        let kind = match mount.source {
            MountSource::Filesystem(_) => "filesystem",
            MountSource::BtrfsSubvolume(_) => "btrfs-subvolume",
            MountSource::ZfsDataset(_) => "zfs-dataset",
            MountSource::Bind(_) => "bind",
            MountSource::Tmpfs => "tmpfs",
            MountSource::Network(_) => "network",
            MountSource::Other(_) => "other",
        };
        *sources.entry(kind).or_default() += 1;
    }

    println!("\nlibmount → CurrentState report");
    println!(
        "provider: {} ({:?})",
        descriptor.manifest().id.as_str(),
        descriptor.backend()
    );
    println!(
        "library: {}",
        descriptor.manifest().library.as_ref().map_or_else(
            || "<none>".into(),
            |library| library.path.display().to_string()
        )
    );
    println!("mount entries: {}", current.mounts.entries.len());
    println!("source classes: {sources:?}");
    println!("diagnostics: {}", current.diagnostics.len());
    for mount in current.mounts.entries.iter().take(20) {
        println!(
            "  target={} source={:?} options={}",
            mount.target.display(),
            mount.source,
            mount.options.join(",")
        );
    }
    if current.mounts.entries.len() > 20 {
        println!("  ... {} more mounts", current.mounts.entries.len() - 20);
    }
    for diagnostic in &current.diagnostics {
        println!(
            "  diagnostic code={} severity={:?}: {}",
            diagnostic.code, diagnostic.severity, diagnostic.message
        );
    }

    assert!(!current.mounts.entries.is_empty());
    assert!(
        current.mounts.entries.iter().any(|mount| {
            mount.context == MountContext::Host && mount.target.as_os_str() == "/"
        })
    );
    assert!(
        !current
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.ends_with(".probe_failed"))
    );
}

#[test]
#[ignore = "requires PDISKS_PROVIDER_MANIFEST, root, and installed udev/fdisk/blkid/libmount"]
/// Runs the complete native block read-path and reports graph/mount correlation.
fn native_block_stack_populates_connected_current_state() {
    let registry = registry();
    let block = NativeBlockProvider::new(
        provider(&registry, "udev-block"),
        provider(&registry, "fdisk-partitions"),
        provider(&registry, "blkid-signatures"),
    );
    let mounts = LibmountProvider::new(provider(&registry, "libmount-mounts"));
    let current = assemble::probe(&[&block as &dyn StateProvider, &mounts as &dyn StateProvider]);

    let mut kinds = BTreeMap::<&'static str, usize>::new();
    for (_, node) in current.graph.nodes() {
        let kind = match node.kind.kind {
            NodeKind::Disk => "disk",
            NodeKind::Loop { .. } => "loop",
            NodeKind::PartitionTable(_) => "partition-table",
            NodeKind::Partition { .. } => "partition",
            NodeKind::Filesystem { .. } => "filesystem",
            NodeKind::Swap => "swap",
            NodeKind::LvmPv => "lvm-pv",
            NodeKind::Unknown(_) => "unknown",
            _ => "other",
        };
        *kinds.entry(kind).or_default() += 1;
    }
    let resolved_mounts = current
        .mounts
        .entries
        .iter()
        .filter(|mount| match mount.source {
            MountSource::Filesystem(id) => current.graph.node(&id).is_some(),
            _ => false,
        })
        .count();

    println!("\nudev + fdisk + blkid + libmount → CurrentState report");
    println!("nodes: {} {kinds:?}", current.graph.nodes().len());
    println!("dependencies: {}", current.graph.dependencies().len());
    println!("mounts: {}", current.mounts.entries.len());
    println!("mounts resolved to graph content: {resolved_mounts}");
    println!("diagnostics: {}", current.diagnostics.len());
    for (id, node) in current.graph.nodes().take(30) {
        let paths = node
            .size
            .block
            .as_ref()
            .map(|block| {
                block
                    .paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        println!(
            "  node={} kind={:?} paths={paths}",
            id.as_uuid(),
            node.kind.kind
        );
    }
    for dependency in current.graph.dependencies().take(30) {
        println!(
            "  edge={} -> {} {:?}",
            dependency.from.as_uuid(),
            dependency.to.as_uuid(),
            dependency.kind
        );
    }
    for diagnostic in &current.diagnostics {
        println!(
            "  diagnostic code={} severity={:?}: {}",
            diagnostic.code, diagnostic.severity, diagnostic.message
        );
    }

    assert!(
        current
            .graph
            .nodes()
            .any(|(_, node)| matches!(node.kind.kind, NodeKind::Disk))
    );
    assert!(current.graph.dependencies().len() > 0);
    assert!(
        resolved_mounts > 0,
        "no libmount source matched blkid content"
    );
    assert!(
        !current
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.ends_with(".probe_failed"))
    );
}

#[test]
#[ignore = "requires PDISKS_PROVIDER_MANIFEST, root, and installed block/dm/LUKS libraries"]
/// Runs libdevmapper and libcryptsetup over the native block graph.
fn native_mapper_stack_populates_current_state() {
    let registry = registry();
    let mapper = NativeMapperProvider::new(
        provider(&registry, "udev-block"),
        provider(&registry, "fdisk-partitions"),
        provider(&registry, "blkid-signatures"),
        provider(&registry, "devmapper-topology"),
        provider(&registry, "cryptsetup-luks"),
    );
    let mounts = LibmountProvider::new(provider(&registry, "libmount-mounts"));
    let current = assemble::probe(&[&mapper as &dyn StateProvider, &mounts as &dyn StateProvider]);

    let mut kinds = BTreeMap::<&'static str, usize>::new();
    for (_, node) in current.graph.nodes() {
        let kind = match &node.kind.kind {
            NodeKind::LuksContainer { .. } => "luks-container",
            NodeKind::DmCryptMapping { .. } => "dm-crypt",
            NodeKind::DmIntegrity { .. } => "dm-integrity",
            NodeKind::DmVerity { .. } => "dm-verity",
            NodeKind::DmMultipath => "dm-multipath",
            NodeKind::DmWritecache { .. } => "dm-writecache",
            NodeKind::Unknown(value) if value.starts_with("device-mapper:") => "dm-other",
            _ => continue,
        };
        *kinds.entry(kind).or_default() += 1;
    }

    println!("\nlibdevmapper + libcryptsetup → CurrentState report");
    println!("classified nodes: {kinds:?}");
    println!("total nodes: {}", current.graph.nodes().len());
    println!("dependencies: {}", current.graph.dependencies().len());
    println!("diagnostics: {}", current.diagnostics.len());
    for (id, node) in current.graph.nodes().filter(|(_, node)| {
        matches!(
            node.kind.kind,
            NodeKind::LuksContainer { .. }
                | NodeKind::DmCryptMapping { .. }
                | NodeKind::DmIntegrity { .. }
                | NodeKind::DmVerity { .. }
                | NodeKind::DmMultipath
                | NodeKind::DmWritecache { .. }
        )
    }) {
        println!("  node={} kind={:?}", id.as_uuid(), node.kind.kind);
    }
    for diagnostic in &current.diagnostics {
        println!(
            "  diagnostic code={} severity={:?}: {}",
            diagnostic.code, diagnostic.severity, diagnostic.message
        );
    }

    assert!(
        current
            .graph
            .nodes()
            .any(|(_, node)| matches!(node.kind.kind, NodeKind::Disk))
    );
    assert!(
        !current
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.ends_with(".probe_failed"))
    );
}

#[test]
#[ignore = "requires PDISKS_PROVIDER_MANIFEST, root, and all implemented native providers"]
/// Runs the complete currently implemented local-storage provider stack.
fn native_local_storage_stack_populates_current_state() {
    let registry = registry();
    let local = NativeLocalProvider::new(
        provider(&registry, "udev-block"),
        provider(&registry, "fdisk-partitions"),
        provider(&registry, "blkid-signatures"),
        provider(&registry, "devmapper-topology"),
        provider(&registry, "cryptsetup-luks"),
        provider(&registry, "loop"),
        provider(&registry, "swap"),
        provider(&registry, "multipath"),
        provider(&registry, "lvm"),
        provider(&registry, "mdraid"),
    );
    let mounts = LibmountProvider::new(provider(&registry, "libmount-mounts"));
    let current = assemble::probe(&[&local as &dyn StateProvider, &mounts as &dyn StateProvider]);

    let mut kinds = BTreeMap::<String, usize>::new();
    for (_, node) in current.graph.nodes() {
        let kind = match &node.kind.kind {
            NodeKind::Disk => "disk",
            NodeKind::Loop {
                backing_file: Some(_),
            } => "loop-backed",
            NodeKind::Loop { backing_file: None } => "loop-unused",
            NodeKind::PartitionTable(_) => "partition-table",
            NodeKind::Partition { .. } => "partition",
            NodeKind::Filesystem { .. } => "filesystem",
            NodeKind::Swap => "swap",
            NodeKind::LuksContainer { .. } => "luks-container",
            NodeKind::DmCryptMapping { .. } => "dm-crypt",
            NodeKind::DmIntegrity { .. } => "dm-integrity",
            NodeKind::DmVerity { .. } => "dm-verity",
            NodeKind::DmMultipath => "dm-multipath",
            NodeKind::DmWritecache { .. } => "dm-writecache",
            NodeKind::LvmPv => "lvm-pv",
            NodeKind::LvmVg { .. } => "lvm-vg",
            NodeKind::LvmLv(_) => "lvm-lv",
            NodeKind::MdArray(_) => "md-array",
            NodeKind::Unknown(value) => value.as_str(),
            _ => "other",
        };
        *kinds.entry(kind.to_owned()).or_default() += 1;
    }

    println!("\nComplete native local-storage → CurrentState report");
    println!("nodes: {} {kinds:?}", current.graph.nodes().len());
    println!("dependencies: {}", current.graph.dependencies().len());
    println!("mounts: {}", current.mounts.entries.len());
    println!("diagnostics: {}", current.diagnostics.len());
    for diagnostic in &current.diagnostics {
        println!(
            "  diagnostic code={} severity={:?}: {} evidence={}",
            diagnostic.code,
            diagnostic.severity,
            diagnostic.message,
            diagnostic.evidence.as_deref().unwrap_or("-")
        );
    }

    assert!(kinds.get("disk").copied().unwrap_or_default() > 0);
    assert!(
        !current
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.ends_with(".probe_failed"))
    );
}

#[test]
#[ignore = "requires PDISKS_PROVIDER_MANIFEST, root, and every CurrentState native provider"]
/// Runs every native CurrentState adapter as one connected provider.
fn native_system_storage_stack_populates_current_state() {
    let registry = registry();
    let local = NativeLocalProvider::new(
        provider(&registry, "udev-block"),
        provider(&registry, "fdisk-partitions"),
        provider(&registry, "blkid-signatures"),
        provider(&registry, "devmapper-topology"),
        provider(&registry, "cryptsetup-luks"),
        provider(&registry, "loop"),
        provider(&registry, "swap"),
        provider(&registry, "multipath"),
        provider(&registry, "lvm"),
        provider(&registry, "mdraid"),
    );
    let system = NativeSystemProvider::new(
        local,
        provider(&registry, "libmount-mounts"),
        provider(&registry, "btrfs"),
        provider(&registry, "zfs"),
        provider(&registry, "nvme"),
    );
    let current = assemble::probe(&[&system as &dyn StateProvider]);

    let mut kinds = BTreeMap::<&'static str, usize>::new();
    for (_, node) in current.graph.nodes() {
        let kind = match &node.kind.kind {
            NodeKind::BtrfsSubvolume { .. } => "btrfs-subvolume",
            NodeKind::ZfsPool { .. } => "zfs-pool",
            NodeKind::ZfsDataset { .. } => "zfs-dataset",
            NodeKind::ZfsVolume { .. } => "zfs-volume",
            NodeKind::NvmeSubsystem { .. } => "nvme-subsystem",
            NodeKind::NvmeController => "nvme-controller",
            NodeKind::NvmeNamespace(_) => "nvme-namespace",
            NodeKind::Disk => "disk",
            _ => "other",
        };
        *kinds.entry(kind).or_default() += 1;
    }

    println!("\nComplete native system-storage → CurrentState report");
    println!("nodes: {} {kinds:?}", current.graph.nodes().len());
    println!("dependencies: {}", current.graph.dependencies().len());
    println!("relations: {}", current.graph.relations().len());
    println!("mounts: {}", current.mounts.entries.len());
    println!("diagnostics: {}", current.diagnostics.len());
    for diagnostic in &current.diagnostics {
        println!(
            "  diagnostic code={} severity={:?}: {} evidence={}",
            diagnostic.code,
            diagnostic.severity,
            diagnostic.message,
            diagnostic.evidence.as_deref().unwrap_or("-")
        );
    }

    assert!(current.graph.nodes().len() > 0);
    assert!(!current.mounts.entries.is_empty());
    assert!(
        !current
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.ends_with(".probe_failed"))
    );
}
