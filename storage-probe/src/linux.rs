//! Linux probe orchestration and fixture-backed CLI adapter.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    path::{Path, PathBuf},
};

use storage_core::model::{
    BlockFacts, BlockGeometry, BlockSize, BtrfsAllocation, Bytes, Dependency, DependencyKind,
    DeviceInfo, DeviceNumber, Diagnostic, DiagnosticSeverity, DiagnosticSubject, ExternalId,
    FilesystemKind, LuksVersion, LvmLv, LvmLvKind, MdArray, MdExternalMetadata, MdMember,
    MdMemberRole, MdMetadata, MdNativeMetadata, MdPersonality, MdRaid, MdRaid0Layout, MemberRole,
    MountContext, MountSource, NetworkMountSource, Node, NodeFacts, NodeGraph, NodeId, NodeKind,
    NodeSpec, NvmeLbaFormat, NvmeNamespace, NvmeNamespaceId, ObservedMount, PartitionAttributes,
    PartitionTable, Presence, Relation, RelationKind, Transport,
};
use storage_provider::{ProviderBackend, RegisteredProvider, UdevBlockEntry};
use uuid::Uuid;

use crate::{
    assemble::{self, ProviderProbeError, ProviderState, StateProvider},
    error::Result,
    parse::{LsblkDevice, LsblkOutput, mounts::MountsOutput, sysfs::SysfsOutput},
    snapshot::ProbeSnapshot,
};

/// Linux current-state probe backed by provider-specific adapters.
pub struct LinuxProbe {
    providers: Vec<Box<dyn StateProvider>>,
    generation: u64,
}

/// Native libmount adapter responsible only for runtime mount discovery.
pub struct LibmountProvider<'a> {
    provider: &'a RegisteredProvider,
}

/// Native adapter joining udev endpoints, fdisk topology, and blkid content.
pub struct NativeBlockProvider<'a> {
    udev: &'a RegisteredProvider,
    fdisk: &'a RegisteredProvider,
    blkid: &'a RegisteredProvider,
}

/// Native adapter extending the block graph with dm and LUKS topology.
pub struct NativeMapperProvider<'a> {
    block: NativeBlockProvider<'a>,
    devmapper: &'a RegisteredProvider,
    cryptsetup: &'a RegisteredProvider,
}

/// Native adapter extending mapper state with loop, swap, and multipath data.
pub struct NativeRuntimeProvider<'a> {
    mapper: NativeMapperProvider<'a>,
    loop_provider: &'a RegisteredProvider,
    swap: &'a RegisteredProvider,
    multipath: &'a RegisteredProvider,
}

/// Complete native adapter for the implemented Linux local-storage stack.
pub struct NativeLocalProvider<'a> {
    runtime: NativeRuntimeProvider<'a>,
    lvm: &'a RegisteredProvider,
    mdraid: &'a RegisteredProvider,
}

/// Native adapter adding Btrfs, ZFS, NVMe, and mounts to local-storage state.
pub struct NativeSystemProvider<'a> {
    local: NativeLocalProvider<'a>,
    libmount: &'a RegisteredProvider,
    btrfs: &'a RegisteredProvider,
    zfs: &'a RegisteredProvider,
    nvme: &'a RegisteredProvider,
}

impl<'a> NativeSystemProvider<'a> {
    /// Extends an assembled local provider with the remaining native sources.
    pub const fn new(
        local: NativeLocalProvider<'a>,
        libmount: &'a RegisteredProvider,
        btrfs: &'a RegisteredProvider,
        zfs: &'a RegisteredProvider,
        nvme: &'a RegisteredProvider,
    ) -> Self {
        Self {
            local,
            libmount,
            btrfs,
            zfs,
            nvme,
        }
    }
}

impl<'a> NativeLocalProvider<'a> {
    /// Binds every native provider currently feeding local-storage state.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        udev: &'a RegisteredProvider,
        fdisk: &'a RegisteredProvider,
        blkid: &'a RegisteredProvider,
        devmapper: &'a RegisteredProvider,
        cryptsetup: &'a RegisteredProvider,
        loop_provider: &'a RegisteredProvider,
        swap: &'a RegisteredProvider,
        multipath: &'a RegisteredProvider,
        lvm: &'a RegisteredProvider,
        mdraid: &'a RegisteredProvider,
    ) -> Self {
        Self {
            runtime: NativeRuntimeProvider::new(
                udev,
                fdisk,
                blkid,
                devmapper,
                cryptsetup,
                loop_provider,
                swap,
                multipath,
            ),
            lvm,
            mdraid,
        }
    }
}

impl<'a> NativeRuntimeProvider<'a> {
    /// Binds native providers required by the current runtime topology layer.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        udev: &'a RegisteredProvider,
        fdisk: &'a RegisteredProvider,
        blkid: &'a RegisteredProvider,
        devmapper: &'a RegisteredProvider,
        cryptsetup: &'a RegisteredProvider,
        loop_provider: &'a RegisteredProvider,
        swap: &'a RegisteredProvider,
        multipath: &'a RegisteredProvider,
    ) -> Self {
        Self {
            mapper: NativeMapperProvider::new(udev, fdisk, blkid, devmapper, cryptsetup),
            loop_provider,
            swap,
            multipath,
        }
    }
}

impl<'a> NativeMapperProvider<'a> {
    /// Binds all native providers required for block, mapper, and LUKS state.
    pub const fn new(
        udev: &'a RegisteredProvider,
        fdisk: &'a RegisteredProvider,
        blkid: &'a RegisteredProvider,
        devmapper: &'a RegisteredProvider,
        cryptsetup: &'a RegisteredProvider,
    ) -> Self {
        Self {
            block: NativeBlockProvider::new(udev, fdisk, blkid),
            devmapper,
            cryptsetup,
        }
    }
}

impl<'a> NativeBlockProvider<'a> {
    /// Binds the three complementary native block-discovery providers.
    pub const fn new(
        udev: &'a RegisteredProvider,
        fdisk: &'a RegisteredProvider,
        blkid: &'a RegisteredProvider,
    ) -> Self {
        Self { udev, fdisk, blkid }
    }
}

impl StateProvider for NativeBlockProvider<'_> {
    fn id(&self) -> &str {
        "native-linux-block"
    }

    fn responsibility(&self) -> &str {
        "block.current-state"
    }

    fn backend(&self) -> ProviderBackend {
        ProviderBackend::Library
    }

    fn probe(&self) -> std::result::Result<ProviderState, ProviderProbeError> {
        self.probe_with_observations().map(|(state, _, _)| state)
    }
}

impl StateProvider for NativeMapperProvider<'_> {
    fn id(&self) -> &str {
        "native-linux-mapper"
    }

    fn responsibility(&self) -> &str {
        "block.current-state"
    }

    fn backend(&self) -> ProviderBackend {
        ProviderBackend::Library
    }

    fn probe(&self) -> std::result::Result<ProviderState, ProviderProbeError> {
        self.probe_with_observations().map(|(state, _, _, _)| state)
    }
}

impl NativeMapperProvider<'_> {
    /// Probes mapper sources and retains raw observations for upper layers.
    fn probe_with_observations(
        &self,
    ) -> std::result::Result<
        (
            ProviderState,
            Vec<UdevBlockEntry>,
            Vec<storage_provider::BlkidEntry>,
            Vec<storage_provider::DevmapperEntry>,
        ),
        ProviderProbeError,
    > {
        let (mut state, endpoints, signatures) = self.block.probe_with_observations()?;
        let luks_devices = signatures
            .iter()
            .filter(|entry| entry.signature_type.as_deref() == Some("crypto_LUKS"))
            .map(|entry| entry.device.clone())
            .collect::<Vec<_>>();
        let luks = match self.cryptsetup.probe_cryptsetup(&luks_devices) {
            Ok(entries) => entries,
            Err(error) => {
                push_native_failure(&mut state, "cryptsetup", error.to_string());
                Vec::new()
            }
        };
        let mappings = match self.devmapper.probe_devmapper() {
            Ok(entries) => entries,
            Err(error) => {
                push_native_failure(&mut state, "devmapper", error.to_string());
                Vec::new()
            }
        };
        enrich_luks(&mut state.graph, &luks);
        enrich_devmapper(&mut state.graph, &endpoints, &mappings);
        Ok((state, endpoints, signatures, mappings))
    }
}

impl StateProvider for NativeRuntimeProvider<'_> {
    fn id(&self) -> &str {
        "native-linux-runtime"
    }

    fn responsibility(&self) -> &str {
        "block.current-state"
    }

    fn backend(&self) -> ProviderBackend {
        ProviderBackend::Library
    }

    fn probe(&self) -> std::result::Result<ProviderState, ProviderProbeError> {
        self.probe_with_observations().map(|(state, _, _, _)| state)
    }
}

impl NativeRuntimeProvider<'_> {
    /// Probes runtime sources and retains raw observations for storage layers.
    fn probe_with_observations(
        &self,
    ) -> std::result::Result<
        (
            ProviderState,
            Vec<UdevBlockEntry>,
            Vec<storage_provider::BlkidEntry>,
            Vec<storage_provider::DevmapperEntry>,
        ),
        ProviderProbeError,
    > {
        let (mut state, endpoints, signatures, mappings) = self.mapper.probe_with_observations()?;
        let loops = endpoints
            .iter()
            .filter(|entry| entry.sysname.starts_with("loop"))
            .map(|entry| entry.devnode.clone())
            .collect::<Vec<_>>();
        let swaps = signatures
            .iter()
            .filter(|entry| entry.signature_type.as_deref() == Some("swap"))
            .map(|entry| entry.device.clone())
            .collect::<Vec<_>>();
        let loop_entries = match self.loop_provider.probe_loop(&loops) {
            Ok(entries) => entries,
            Err(error) => {
                push_native_failure(&mut state, "loop", error.to_string());
                Vec::new()
            }
        };
        let swap_entries = match self.swap.probe_swap(&swaps) {
            Ok(entries) => entries,
            Err(error) => {
                push_native_failure(&mut state, "swap", error.to_string());
                Vec::new()
            }
        };
        let multipath = match self.multipath.probe_multipath() {
            Ok(entry) => entry,
            Err(error) => {
                push_native_failure(&mut state, "multipath", error.to_string());
                storage_provider::MultipathEntry::default()
            }
        };
        enrich_loop(&mut state.graph, &loop_entries);
        enrich_swap(&mut state, &swap_entries);
        enrich_multipath(&mut state, &mappings, &multipath);
        Ok((state, endpoints, signatures, mappings))
    }
}

impl StateProvider for NativeLocalProvider<'_> {
    fn id(&self) -> &str {
        "native-linux-local-storage"
    }

    fn responsibility(&self) -> &str {
        "block.current-state"
    }

    fn backend(&self) -> ProviderBackend {
        ProviderBackend::Library
    }

    fn probe(&self) -> std::result::Result<ProviderState, ProviderProbeError> {
        let (mut state, endpoints, signatures, mappings) =
            self.runtime.probe_with_observations()?;
        let arrays = endpoints
            .iter()
            .filter(|entry| entry.sysname.starts_with("md"))
            .map(|entry| entry.devnode.clone())
            .collect::<Vec<_>>();
        let members = signatures
            .iter()
            .filter(|entry| entry.signature_type.as_deref() == Some("linux_raid_member"))
            .map(|entry| entry.device.clone())
            .collect::<Vec<_>>();
        let mdraid = match self.mdraid.probe_mdraid(&arrays, &members) {
            Ok(entry) => entry,
            Err(error) => {
                push_native_failure(&mut state, "mdraid", error.to_string());
                storage_provider::MdraidEntry::default()
            }
        };
        let lvm = match self.lvm.probe_lvm() {
            Ok(entry) => entry,
            Err(error) => {
                push_native_failure(&mut state, "lvm", error.to_string());
                storage_provider::LvmEntry::default()
            }
        };
        enrich_mdraid(&mut state.graph, &mdraid);
        enrich_lvm(&mut state.graph, &endpoints, &mappings, &lvm);
        Ok(state)
    }
}

impl StateProvider for NativeSystemProvider<'_> {
    fn id(&self) -> &str {
        "native-linux-system-storage"
    }

    fn responsibility(&self) -> &str {
        "storage.current-state"
    }

    fn backend(&self) -> ProviderBackend {
        ProviderBackend::Library
    }

    fn probe(&self) -> std::result::Result<ProviderState, ProviderProbeError> {
        let mut state = self.local.probe()?;
        let mount_entries = match self.libmount.probe_libmount() {
            Ok(entries) => entries,
            Err(error) => {
                push_native_failure(&mut state, "libmount", error.to_string());
                Vec::new()
            }
        };
        let btrfs_mountpoints = mount_entries
            .iter()
            .filter(|entry| entry.filesystem_type == "btrfs")
            .map(|entry| entry.target.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        state.mounts = mount_entries.into_iter().map(observed_mount).collect();

        let btrfs = match self.btrfs.probe_btrfs(&btrfs_mountpoints) {
            Ok(entries) => entries,
            Err(error) => {
                push_native_failure(&mut state, "btrfs", error.to_string());
                Vec::new()
            }
        };
        let zfs = match self.zfs.probe_zfs() {
            Ok(entry) => entry,
            Err(error) => {
                push_native_failure(&mut state, "zfs", error.to_string());
                storage_provider::ZfsEntry::default()
            }
        };
        let nvme = match self.nvme.probe_nvme() {
            Ok(entry) => entry,
            Err(error) => {
                push_native_failure(&mut state, "nvme", error.to_string());
                storage_provider::NvmeEntry::default()
            }
        };
        enrich_btrfs(&mut state.graph, &state.mounts, &btrfs);
        enrich_zfs(&mut state.graph, &zfs);
        enrich_nvme(&mut state.graph, &nvme);
        Ok(state)
    }
}

impl NativeBlockProvider<'_> {
    /// Probes block sources and retains raw observations for upper layers.
    fn probe_with_observations(
        &self,
    ) -> std::result::Result<
        (
            ProviderState,
            Vec<UdevBlockEntry>,
            Vec<storage_provider::BlkidEntry>,
        ),
        ProviderProbeError,
    > {
        let endpoints = self
            .udev
            .probe_udev_blocks()
            .map_err(|error| ProviderProbeError::new("udev.probe_failed", error.to_string()))?;
        let disks = endpoints
            .iter()
            .filter(|entry| entry.devtype.as_deref() == Some("disk"))
            .map(|entry| entry.devnode.clone())
            .collect::<Vec<_>>();
        let tables = self
            .fdisk
            .probe_fdisk_partitions(&disks)
            .map_err(|error| ProviderProbeError::new("fdisk.probe_failed", error.to_string()))?;
        let signatures = self
            .blkid
            .probe_blkid_signatures()
            .map_err(|error| ProviderProbeError::new("blkid.probe_failed", error.to_string()))?;
        let state = assemble_native_block_state(&endpoints, &tables, &signatures);
        Ok((state, endpoints, signatures))
    }
}

impl<'a> LibmountProvider<'a> {
    /// Binds the adapter to a library-backed registry entry.
    pub const fn new(provider: &'a RegisteredProvider) -> Self {
        Self { provider }
    }
}

impl StateProvider for LibmountProvider<'_> {
    fn id(&self) -> &str {
        self.provider.manifest().id.as_str()
    }

    fn responsibility(&self) -> &str {
        "mounts.runtime"
    }

    fn backend(&self) -> ProviderBackend {
        self.provider.backend()
    }

    fn probe(&self) -> std::result::Result<ProviderState, ProviderProbeError> {
        let mounts = self
            .provider
            .probe_libmount()
            .map_err(|error| ProviderProbeError::new("libmount.probe_failed", error.to_string()))?
            .into_iter()
            .map(observed_mount)
            .collect();
        Ok(ProviderState {
            mounts,
            ..ProviderState::default()
        })
    }
}

/// Converts one native libmount record into the canonical mount model.
fn observed_mount(entry: storage_provider::LibmountEntry) -> ObservedMount {
    let source = entry.source.to_string_lossy();
    ObservedMount {
        source: if source.starts_with("/dev/") {
            MountSource::Filesystem(content_node_id(&source))
        } else {
            external_mount_source(&source, &entry.filesystem_type)
        },
        target: entry.target,
        options: entry.options,
        context: MountContext::Host,
    }
}

/// Joins complementary native observations into one non-conflicting graph.
fn assemble_native_block_state(
    endpoints: &[UdevBlockEntry],
    tables: &[storage_provider::FdiskTable],
    signatures: &[storage_provider::BlkidEntry],
) -> ProviderState {
    let by_path = endpoints
        .iter()
        .map(|entry| (entry.devnode.clone(), entry))
        .collect::<HashMap<_, _>>();
    let mut graph = NodeGraph::new();
    let mut represented = HashSet::new();

    for endpoint in endpoints
        .iter()
        .filter(|entry| entry.devtype.as_deref() != Some("partition"))
    {
        insert_native_endpoint(&mut graph, endpoint, native_endpoint_kind(endpoint));
        represented.insert(endpoint.devnode.clone());
    }

    for table in tables.iter().filter(|table| table.label.is_some()) {
        let disk_id = block_node_id(&table.device);
        let Some(table_kind) = table.label.as_deref().and_then(native_partition_table) else {
            continue;
        };
        let table_id = stable_node_id("partition-table-path", &table.device.to_string_lossy());
        graph.insert_node(
            table_id,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::PartitionTable(table_kind),
                    size: None,
                },
                size: NodeFacts {
                    presence: Presence::Present,
                    ..NodeFacts::default()
                },
            },
        );
        graph.insert_dependency(Dependency {
            from: disk_id,
            to: table_id,
            kind: DependencyKind::Backs,
        });
        for partition in &table.partitions {
            let Some(number) = NonZeroU32::new(partition.number) else {
                continue;
            };
            let path = partition_path(&table.device, partition.number);
            let endpoint = by_path.get(&path).copied();
            let offset = Bytes::new(partition.start_sectors.saturating_mul(table.sector_size));
            let id = block_node_id(&path);
            let mut facts = endpoint.map_or_else(NodeFacts::default, native_block_facts);
            facts.presence = Presence::Present;
            if let Some(uuid) = partition
                .uuid
                .as_deref()
                .and_then(|value| Uuid::parse_str(value).ok())
            {
                facts.identities.push(ExternalId::GptPartitionUuid(uuid));
            }
            graph.insert_node(
                id,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Partition {
                            number,
                            offset,
                            role: None,
                            attributes: PartitionAttributes {
                                gpt: None,
                                bootable: Some(partition.bootable),
                            },
                        },
                        size: Some(Bytes::new(
                            partition.size_sectors.saturating_mul(table.sector_size),
                        )),
                    },
                    size: facts,
                },
            );
            graph.insert_dependency(Dependency {
                from: table_id,
                to: id,
                kind: DependencyKind::Contains { offset },
            });
            represented.insert(path);
        }
    }

    for endpoint in endpoints
        .iter()
        .filter(|entry| !represented.contains(&entry.devnode))
    {
        insert_native_endpoint(
            &mut graph,
            endpoint,
            NodeKind::Unknown("block endpoint without fdisk topology".to_owned()),
        );
    }

    for signature in signatures {
        if graph.node(&block_node_id(&signature.device)).is_none() {
            continue;
        }
        add_native_content(
            &mut graph,
            signature,
            by_path.get(&signature.device).copied(),
        );
    }
    ProviderState {
        graph,
        ..ProviderState::default()
    }
}

/// Preserves partial state while recording one failed native enrichment.
fn push_native_failure(state: &mut ProviderState, provider: &str, evidence: String) {
    state.diagnostics.push(Diagnostic {
        code: format!("{provider}.probe_failed"),
        severity: DiagnosticSeverity::MissingInformation,
        subjects: Vec::new(),
        message: format!("native {provider} enrichment could not inspect its responsibility"),
        evidence: Some(evidence),
        suggested_remedy: Some("fix provider access and repeat probing".to_owned()),
    });
}

/// Replaces blkid's generic LUKS signature nodes with cryptsetup metadata.
fn enrich_luks(graph: &mut NodeGraph, entries: &[storage_provider::CryptsetupEntry]) {
    for entry in entries {
        let id = content_node_id(&entry.device.to_string_lossy());
        let Some(mut node) = graph.node(&id).cloned() else {
            continue;
        };
        let version = match entry.luks_type.as_str() {
            "LUKS1" => LuksVersion::Luks1,
            "LUKS2" => LuksVersion::Luks2,
            _ => continue,
        };
        node.kind.kind = NodeKind::LuksContainer { version };
        node.size.identities.retain(|identity| {
            !matches!(identity, ExternalId::Filesystem { fs_type, .. } if fs_type == "crypto_LUKS")
        });
        if let Some(uuid) = entry
            .uuid
            .as_deref()
            .and_then(|value| Uuid::parse_str(value).ok())
        {
            node.size.identities.push(ExternalId::LuksUuid(uuid));
        }
        graph.insert_node(id, node);
    }
}

/// Adds device-mapper kinds, aliases, and backing edges to the block graph.
fn enrich_devmapper(
    graph: &mut NodeGraph,
    endpoints: &[UdevBlockEntry],
    entries: &[storage_provider::DevmapperEntry],
) {
    let paths = endpoints
        .iter()
        .map(|entry| ((entry.major, entry.minor), entry.devnode.clone()))
        .collect::<HashMap<_, _>>();
    for entry in entries {
        let path = paths
            .get(&(entry.devno.major, entry.devno.minor))
            .cloned()
            .unwrap_or_else(|| PathBuf::from(format!("/dev/dm-{}", entry.devno.minor)));
        let id = block_node_id(&path);
        let alias = PathBuf::from("/dev/mapper").join(&entry.name);
        let mut node = graph.node(&id).cloned().unwrap_or_else(|| Node {
            kind: NodeSpec {
                kind: NodeKind::Unknown("device-mapper endpoint".to_owned()),
                size: None,
            },
            size: NodeFacts {
                presence: Presence::Present,
                block: Some(BlockFacts {
                    paths: vec![path.clone()],
                    devno: DeviceNumber {
                        major: entry.devno.major,
                        minor: entry.devno.minor,
                    },
                    geometry: None,
                    read_only: Some(entry.read_only),
                }),
                ..NodeFacts::default()
            },
        });
        node.kind.kind = devmapper_kind(entry);
        if let Some(block) = &mut node.size.block {
            if !block.paths.contains(&alias) {
                block.paths.push(alias);
            }
            block.read_only = Some(entry.read_only);
        }
        graph.insert_node(id, node);

        for dependency in &entry.dependencies {
            let Some(source_path) = paths.get(&(dependency.major, dependency.minor)) else {
                continue;
            };
            let block_source = block_node_id(source_path);
            let content_source = content_node_id(&source_path.to_string_lossy());
            let source = if entry.targets.iter().any(|target| target == "crypt")
                && matches!(
                    graph.node(&content_source).map(|node| &node.kind.kind),
                    Some(NodeKind::LuksContainer { .. })
                ) {
                content_source
            } else {
                block_source
            };
            graph.insert_dependency(Dependency {
                from: source,
                to: id,
                kind: if entry.targets.iter().any(|target| target == "multipath") {
                    DependencyKind::Path
                } else {
                    DependencyKind::Backs
                },
            });
        }
    }
}

/// Adds loop backing-file metadata to existing block endpoints.
fn enrich_loop(graph: &mut NodeGraph, entries: &[storage_provider::LoopEntry]) {
    for entry in entries {
        let id = block_node_id(&entry.device);
        let Some(mut node) = graph.node(&id).cloned() else {
            continue;
        };
        node.kind.kind = NodeKind::Loop {
            backing_file: Some(entry.backing_file.clone()),
        };
        if let Some(block) = &mut node.size.block {
            block.read_only = Some(entry.read_only);
        }
        graph.insert_node(id, node);
    }
}

/// Records active swap areas as safety-relevant runtime diagnostics.
fn enrich_swap(state: &mut ProviderState, entries: &[storage_provider::SwapEntry]) {
    state
        .diagnostics
        .extend(entries.iter().filter(|entry| entry.active).map(|entry| {
            let id = content_node_id(&entry.device.to_string_lossy());
            Diagnostic {
                code: "swap.active".to_owned(),
                severity: DiagnosticSeverity::Warning,
                subjects: vec![DiagnosticSubject::Node(id)],
                message: format!("swap area {} is active", entry.device.display()),
                evidence: Some("reported by libblockdev swap plugin".to_owned()),
                suggested_remedy: Some(
                    "deactivate swap before modifying its backing storage".to_owned(),
                ),
            }
        }));
}

/// Reports multipath members not connected to a mapper Path edge.
fn enrich_multipath(
    state: &mut ProviderState,
    mappings: &[storage_provider::DevmapperEntry],
    multipath: &storage_provider::MultipathEntry,
) {
    if !mappings
        .iter()
        .any(|mapping| mapping.targets.iter().any(|target| target == "multipath"))
    {
        return;
    }
    for member in &multipath.members {
        let member_id = block_node_id(member);
        let connected = state.graph.dependencies().any(|dependency| {
            dependency.from == member_id && dependency.kind == DependencyKind::Path
        });
        if !connected {
            state.diagnostics.push(Diagnostic {
                code: "multipath.member_unlinked".to_owned(),
                severity: DiagnosticSeverity::MissingInformation,
                subjects: vec![DiagnosticSubject::Node(member_id)],
                message: format!(
                    "multipath member {} was not linked to a mapper device",
                    member.display()
                ),
                evidence: Some("libblockdev and libdevmapper observations disagree".to_owned()),
                suggested_remedy: Some("repeat probing after multipath settles".to_owned()),
            });
        }
    }
}

/// Adds MD array kinds, identities, and member edges.
fn enrich_mdraid(graph: &mut NodeGraph, topology: &storage_provider::MdraidEntry) {
    for array in &topology.arrays {
        let id = block_node_id(&array.device);
        let mut node = graph.node(&id).cloned().unwrap_or_else(|| Node {
            kind: NodeSpec {
                kind: NodeKind::Unknown("md array".to_owned()),
                size: Some(Bytes::new(array.size)),
            },
            size: NodeFacts {
                presence: Presence::Present,
                ..NodeFacts::default()
            },
        });
        node.kind.kind = mdraid_kind(array, &topology.members);
        node.kind.size = Some(Bytes::new(array.size));
        if let Some(uuid) = &array.uuid {
            let identity = ExternalId::MdUuid(uuid.clone());
            if !node.size.identities.contains(&identity) {
                node.size.identities.push(identity);
            }
        }
        graph.insert_node(id, node);
    }

    for member in &topology.members {
        let Some(array_uuid) = member.array_uuid.as_deref() else {
            continue;
        };
        let Some(array) = topology.arrays.iter().find(|array| {
            array.uuid.as_deref().is_some_and(|uuid| {
                normalized_native_uuid(uuid) == normalized_native_uuid(array_uuid)
            })
        }) else {
            continue;
        };
        graph.insert_dependency(Dependency {
            from: block_node_id(&member.device),
            to: block_node_id(&array.device),
            kind: DependencyKind::MemberOf(MemberRole::Md(MdMember {
                role: MdMemberRole::Data,
                slot: None,
                data_offset: None,
            })),
        });
    }
}

/// Builds the strongest MD kind supported by the available metadata.
fn mdraid_kind(
    array: &storage_provider::MdraidArrayEntry,
    members: &[storage_provider::MdraidMemberEntry],
) -> NodeKind {
    let metadata = md_metadata(array.metadata.as_deref());
    let chunk_size = array
        .uuid
        .as_deref()
        .and_then(|uuid| {
            members.iter().find(|member| {
                member.array_uuid.as_deref().is_some_and(|member_uuid| {
                    normalized_native_uuid(member_uuid) == normalized_native_uuid(uuid)
                })
            })
        })
        .map_or(0, |member| member.chunk_size);
    let personality = match array.level.to_ascii_lowercase().as_str() {
        "raid0" => Some(MdPersonality::Raid(MdRaid::Raid0 {
            layout: MdRaid0Layout::Unspecified,
            chunk_size: Bytes::new(chunk_size),
        })),
        "raid1" => Some(MdPersonality::Raid(MdRaid::Raid1)),
        "raid4" => Some(MdPersonality::Raid(MdRaid::Raid4 {
            chunk_size: Bytes::new(chunk_size),
        })),
        "linear" => Some(MdPersonality::Linear { rounding: None }),
        "multipath" => Some(MdPersonality::Multipath),
        "faulty" => Some(MdPersonality::Faulty),
        "container" => Some(MdPersonality::Container),
        _ => None,
    };
    personality.map_or_else(
        || NodeKind::Unknown(format!("md:{}", array.level)),
        |personality| {
            NodeKind::MdArray(MdArray {
                personality,
                metadata,
            })
        },
    )
}

/// Maps MD superblock names without guessing unknown formats.
fn md_metadata(metadata: Option<&str>) -> MdMetadata {
    match metadata.unwrap_or_default().to_ascii_lowercase().as_str() {
        "0.90" => MdMetadata::Native(MdNativeMetadata::V0_90),
        "1.0" => MdMetadata::Native(MdNativeMetadata::V1_0),
        "1.1" => MdMetadata::Native(MdNativeMetadata::V1_1),
        "1.2" => MdMetadata::Native(MdNativeMetadata::V1_2),
        "ddf" => MdMetadata::External(MdExternalMetadata::Ddf),
        "imsm" => MdMetadata::External(MdExternalMetadata::Imsm),
        _ => MdMetadata::None,
    }
}

/// Adds LVM PV/VG/LV topology and stable LVM identities.
fn enrich_lvm(
    graph: &mut NodeGraph,
    endpoints: &[UdevBlockEntry],
    mappings: &[storage_provider::DevmapperEntry],
    topology: &storage_provider::LvmEntry,
) {
    let endpoint_paths = endpoints
        .iter()
        .map(|entry| ((entry.major, entry.minor), entry.devnode.clone()))
        .collect::<HashMap<_, _>>();
    let vg_ids = topology
        .vgs
        .iter()
        .map(|vg| (vg.name.as_str(), stable_node_id("lvm-vg", &vg.uuid)))
        .collect::<HashMap<_, _>>();

    for vg in &topology.vgs {
        let id = stable_node_id("lvm-vg", &vg.uuid);
        graph.insert_node(
            id,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::LvmVg {
                        name: vg.name.clone(),
                        extent_size: (vg.extent_size > 0).then(|| Bytes::new(vg.extent_size)),
                    },
                    size: Some(Bytes::new(vg.size)),
                },
                size: NodeFacts {
                    presence: Presence::Present,
                    identities: vec![ExternalId::LvmVgUuid(vg.uuid.clone())],
                    ..NodeFacts::default()
                },
            },
        );
    }

    for pv in &topology.pvs {
        let id = content_node_id(&pv.device.to_string_lossy());
        let mut node = graph.node(&id).cloned().unwrap_or_else(|| Node {
            kind: NodeSpec {
                kind: NodeKind::LvmPv,
                size: Some(Bytes::new(pv.size)),
            },
            size: NodeFacts {
                presence: Presence::Present,
                ..NodeFacts::default()
            },
        });
        node.kind.kind = NodeKind::LvmPv;
        let identity = ExternalId::LvmPvUuid(pv.uuid.clone());
        if !node.size.identities.contains(&identity) {
            node.size.identities.push(identity);
        }
        graph.insert_node(id, node);
        if let Some(vg_name) = pv.vg_name.as_deref()
            && let Some(vg_id) = vg_ids.get(vg_name)
        {
            graph.insert_dependency(Dependency {
                from: id,
                to: *vg_id,
                kind: DependencyKind::MemberOf(MemberRole::LvmPv),
            });
        }
    }

    for lv in &topology.lvs {
        let mapping = mappings.iter().find(|mapping| {
            mapping.uuid.as_deref().is_some_and(|uuid| {
                uuid.starts_with("LVM-")
                    && normalized_native_uuid(uuid).ends_with(&normalized_native_uuid(&lv.uuid))
            })
        });
        let active_path = mapping.and_then(|mapping| {
            endpoint_paths
                .get(&(mapping.devno.major, mapping.devno.minor))
                .cloned()
        });
        let id = active_path
            .as_deref()
            .map_or_else(|| stable_node_id("lvm-lv", &lv.uuid), block_node_id);
        let mut facts = graph
            .node(&id)
            .map(|node| node.size.clone())
            .unwrap_or_else(|| NodeFacts {
                presence: Presence::Present,
                ..NodeFacts::default()
            });
        let identity = ExternalId::LvmLvUuid(lv.uuid.clone());
        if !facts.identities.contains(&identity) {
            facts.identities.push(identity);
        }
        graph.insert_node(
            id,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::LvmLv(LvmLv {
                        name: lv.name.clone(),
                        kind: lvm_lv_kind(lv.segment_type.as_deref()),
                    }),
                    size: Some(Bytes::new(lv.size)),
                },
                size: facts,
            },
        );
        if let Some(vg_id) = vg_ids.get(lv.vg_name.as_str()) {
            graph.insert_dependency(Dependency {
                from: *vg_id,
                to: id,
                kind: DependencyKind::Provides,
            });
        }
    }
}

/// Maps LVM segment names that do not require missing geometry fields.
fn lvm_lv_kind(segment_type: Option<&str>) -> LvmLvKind {
    match segment_type.unwrap_or("linear") {
        "linear" => LvmLvKind::Linear,
        "thin-pool" => LvmLvKind::ThinPool,
        "thin" => LvmLvKind::Thin,
        "snapshot" => LvmLvKind::Snapshot,
        "cache-pool" => LvmLvKind::CachePool,
        "cache" => LvmLvKind::Cache,
        "writecache" => LvmLvKind::WriteCache,
        other => LvmLvKind::Other(other.to_owned()),
    }
}

/// Normalizes provider UUID punctuation for cross-library comparison.
fn normalized_native_uuid(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Adds Btrfs subvolumes below their mounted filesystem nodes.
fn enrich_btrfs(
    graph: &mut NodeGraph,
    mounts: &[ObservedMount],
    filesystems: &[storage_provider::BtrfsEntry],
) {
    for filesystem in filesystems {
        let Some(filesystem_id) = mounts.iter().find_map(|mount| {
            (mount.target == filesystem.mountpoint)
                .then_some(&mount.source)
                .and_then(|source| match source {
                    MountSource::Filesystem(id) => Some(*id),
                    _ => None,
                })
        }) else {
            continue;
        };
        for subvolume in &filesystem.subvolumes {
            let id = stable_node_id(
                "btrfs-subvolume",
                &format!("{}:{}", filesystem_id.as_uuid(), subvolume.id),
            );
            graph.insert_node(
                id,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::BtrfsSubvolume {
                            name: subvolume.path.to_string_lossy().into_owned(),
                            read_only: subvolume.read_only,
                            is_default: subvolume.is_default,
                        },
                        size: None,
                    },
                    size: NodeFacts {
                        presence: Presence::Present,
                        ..NodeFacts::default()
                    },
                },
            );
            graph.insert_dependency(Dependency {
                from: filesystem_id,
                to: id,
                kind: DependencyKind::Provides,
            });
        }
    }
}

/// Adds imported ZFS pools, filesystems, and volumes.
fn enrich_zfs(graph: &mut NodeGraph, topology: &storage_provider::ZfsEntry) {
    let pools = topology
        .pools
        .iter()
        .map(|name| (name.as_str(), stable_node_id("zfs-pool", name)))
        .collect::<HashMap<_, _>>();
    for (name, id) in &pools {
        graph.insert_node(
            *id,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::ZfsPool {
                        name: (*name).to_owned(),
                    },
                    size: None,
                },
                size: NodeFacts {
                    presence: Presence::Present,
                    ..NodeFacts::default()
                },
            },
        );
    }
    for dataset in &topology.datasets {
        let id = stable_node_id("zfs-dataset", &dataset.name);
        let kind = match dataset.kind {
            storage_provider::ZfsDatasetKind::Filesystem => NodeKind::ZfsDataset {
                name: dataset.name.clone(),
            },
            storage_provider::ZfsDatasetKind::Volume => NodeKind::ZfsVolume {
                name: dataset.name.clone(),
            },
        };
        graph.insert_node(
            id,
            Node {
                kind: NodeSpec { kind, size: None },
                size: NodeFacts {
                    presence: Presence::Present,
                    ..NodeFacts::default()
                },
            },
        );
        if let Some(pool_name) = dataset.name.split('/').next()
            && let Some(pool_id) = pools.get(pool_name)
        {
            graph.insert_dependency(Dependency {
                from: *pool_id,
                to: id,
                kind: DependencyKind::Provides,
            });
        }
    }
}

/// Reclassifies NVMe endpoints and adds subsystem/controller topology.
fn enrich_nvme(graph: &mut NodeGraph, topology: &storage_provider::NvmeEntry) {
    for subsystem in &topology.subsystems {
        let subsystem_id = stable_node_id("nvme-subsystem", &subsystem.nqn);
        graph.insert_node(
            subsystem_id,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::NvmeSubsystem {
                        nqn: subsystem.nqn.clone(),
                    },
                    size: None,
                },
                size: NodeFacts {
                    presence: Presence::Present,
                    ..NodeFacts::default()
                },
            },
        );
        let transport = subsystem
            .controllers
            .iter()
            .find_map(|controller| controller.transport.as_deref())
            .map(|transport| {
                if transport == "pcie" {
                    Transport::Nvme
                } else {
                    Transport::NvmeOf
                }
            });
        for controller in &subsystem.controllers {
            let controller_id = stable_node_id("nvme-controller", &controller.name);
            graph.insert_node(
                controller_id,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::NvmeController,
                        size: None,
                    },
                    size: NodeFacts {
                        presence: Presence::Present,
                        ..NodeFacts::default()
                    },
                },
            );
            graph.insert_relation(Relation {
                from: controller_id,
                to: subsystem_id,
                kind: RelationKind::NvmeAttachedTo,
            });
        }
        for namespace in &subsystem.namespaces {
            let (Some(nsid), Some(data_size)) = (
                NvmeNamespaceId::new(namespace.nsid),
                BlockSize::new(namespace.lba_size),
            ) else {
                continue;
            };
            let path = PathBuf::from("/dev").join(&namespace.name);
            let id = block_node_id(&path);
            let mut node = graph.node(&id).cloned().unwrap_or_else(|| Node {
                kind: NodeSpec {
                    kind: NodeKind::Unknown("nvme namespace".to_owned()),
                    size: None,
                },
                size: NodeFacts {
                    presence: Presence::Present,
                    ..NodeFacts::default()
                },
            });
            node.kind.kind = NodeKind::NvmeNamespace(NvmeNamespace {
                nsid,
                format: NvmeLbaFormat {
                    data_size,
                    metadata_size: namespace.metadata_size,
                },
            });
            if let Some(device) = &mut node.size.device {
                device.transport = transport;
            }
            for identity in nvme_identities(namespace) {
                if !node.size.identities.contains(&identity) {
                    node.size.identities.push(identity);
                }
            }
            graph.insert_node(id, node);
            graph.insert_dependency(Dependency {
                from: subsystem_id,
                to: id,
                kind: DependencyKind::Provides,
            });
        }
    }
}

/// Converts non-zero libnvme identifiers into canonical external IDs.
fn nvme_identities(namespace: &storage_provider::NvmeNamespaceEntry) -> Vec<ExternalId> {
    let mut identities = Vec::new();
    if let Some(nguid) = namespace.nguid {
        identities.push(ExternalId::NvmeNguid(nguid));
    }
    if let Some(eui64) = namespace.eui64 {
        identities.push(ExternalId::NvmeEui64(eui64));
    }
    if let Some(uuid) = namespace.uuid {
        identities.push(ExternalId::NvmeUuid(Uuid::from_bytes(uuid)));
    }
    identities
}

/// Classifies a live mapping from its kernel target types.
fn devmapper_kind(entry: &storage_provider::DevmapperEntry) -> NodeKind {
    if entry.targets.iter().any(|target| target == "crypt") {
        NodeKind::DmCryptMapping {
            name: entry.name.clone(),
        }
    } else if entry.targets.iter().any(|target| target == "integrity") {
        NodeKind::DmIntegrity {
            name: entry.name.clone(),
        }
    } else if entry.targets.iter().any(|target| target == "verity") {
        NodeKind::DmVerity {
            name: entry.name.clone(),
        }
    } else if entry.targets.iter().any(|target| target == "multipath") {
        NodeKind::DmMultipath
    } else if entry.targets.iter().any(|target| target == "writecache") {
        NodeKind::DmWritecache {
            name: entry.name.clone(),
        }
    } else {
        NodeKind::Unknown(format!("device-mapper:{}", entry.targets.join("+")))
    }
}

/// Inserts one endpoint and its udev-backed runtime facts.
fn insert_native_endpoint(graph: &mut NodeGraph, endpoint: &UdevBlockEntry, kind: NodeKind) {
    graph.insert_node(
        block_node_id(&endpoint.devnode),
        Node {
            kind: NodeSpec {
                kind,
                size: endpoint
                    .size_sectors
                    .map(|sectors| Bytes::new(sectors.saturating_mul(512))),
            },
            size: native_block_facts(endpoint),
        },
    );
}

/// Builds canonical runtime facts from one libudev endpoint.
fn native_block_facts(endpoint: &UdevBlockEntry) -> NodeFacts {
    let mut identities = Vec::new();
    if let Some(wwn) = endpoint.wwn.clone() {
        identities.push(ExternalId::Wwn(wwn));
    }
    if let Some(serial) = endpoint.serial.clone() {
        identities.push(ExternalId::Serial {
            vendor: None,
            value: serial,
        });
    }
    NodeFacts {
        presence: Presence::Present,
        identities,
        block: Some(BlockFacts {
            paths: vec![endpoint.devnode.clone()],
            devno: DeviceNumber {
                major: endpoint.major,
                minor: endpoint.minor,
            },
            geometry: endpoint.logical_block_size.and_then(BlockSize::new).map(
                |logical_block_size| BlockGeometry {
                    logical_block_size,
                    physical_block_size: endpoint.physical_block_size.and_then(BlockSize::new),
                    alignment_offset: None,
                    minimum_io_size: None,
                    optimal_io_size: None,
                },
            ),
            read_only: endpoint.read_only,
        }),
        device: (endpoint.devtype.as_deref() == Some("disk")).then(|| DeviceInfo {
            model: endpoint.model.clone(),
            vendor: None,
            transport: transport(&endpoint.sysname),
            network_backing: None,
            rotational: endpoint.rotational,
            removable: endpoint.removable,
            zoned: None,
        }),
        ..NodeFacts::default()
    }
}

/// Maps a native endpoint to the most specific kind owned by the block layer.
fn native_endpoint_kind(endpoint: &UdevBlockEntry) -> NodeKind {
    if endpoint.sysname.starts_with("loop") {
        NodeKind::Loop { backing_file: None }
    } else if endpoint.sysname.starts_with("zram") {
        NodeKind::Zram
    } else if endpoint.devtype.as_deref() == Some("disk") {
        NodeKind::Disk
    } else {
        NodeKind::Unknown("udev block endpoint".to_owned())
    }
}

/// Adds content identified by libblkid above its backing endpoint.
fn add_native_content(
    graph: &mut NodeGraph,
    signature: &storage_provider::BlkidEntry,
    endpoint: Option<&UdevBlockEntry>,
) {
    let Some(signature_type) = signature.signature_type.as_deref() else {
        return;
    };
    let kind = match signature_type {
        "swap" => NodeKind::Swap,
        "LVM2_member" => NodeKind::LvmPv,
        "crypto_LUKS" | "linux_raid_member" => {
            NodeKind::Unknown(format!("signature:{signature_type}"))
        }
        other => NodeKind::Filesystem {
            kind: filesystem_kind(other),
            label: signature.label.clone(),
        },
    };
    let id = content_node_id(&signature.device.to_string_lossy());
    let identities = signature.uuid.as_ref().map_or_else(Vec::new, |uuid| {
        vec![ExternalId::Filesystem {
            fs_type: signature_type.to_owned(),
            value: uuid.clone(),
        }]
    });
    graph.insert_node(
        id,
        Node {
            kind: NodeSpec {
                kind,
                size: endpoint
                    .and_then(|entry| entry.size_sectors)
                    .map(|sectors| Bytes::new(sectors.saturating_mul(512))),
            },
            size: NodeFacts {
                presence: Presence::Present,
                identities,
                ..NodeFacts::default()
            },
        },
    );
    graph.insert_dependency(Dependency {
        from: block_node_id(&signature.device),
        to: id,
        kind: DependencyKind::Backs,
    });
}

/// Maps libfdisk label names to the canonical table kind.
fn native_partition_table(label: &str) -> Option<PartitionTable> {
    match label {
        "gpt" => Some(PartitionTable::Gpt),
        "dos" => Some(PartitionTable::Mbr),
        _ => None,
    }
}

/// Derives the conventional Linux partition device path.
fn partition_path(device: &Path, number: u32) -> PathBuf {
    let mut path = device.as_os_str().to_os_string();
    if device
        .as_os_str()
        .as_encoded_bytes()
        .last()
        .is_some_and(u8::is_ascii_digit)
    {
        path.push("p");
    }
    path.push(number.to_string());
    PathBuf::from(path)
}

impl LinuxProbe {
    /// Creates a probe from adapters with non-overlapping responsibilities.
    pub fn from_providers(providers: Vec<Box<dyn StateProvider>>) -> Self {
        Self {
            providers,
            generation: 1,
        }
    }

    /// Creates a test probe from recorded CLI outputs.
    ///
    /// Fixtures intentionally exercise recorded CLI DTO parsing; production
    /// callers should prefer native adapters and register CLI adapters only for
    /// responsibilities unavailable through a library, using
    /// [`LinuxProbe::from_providers`].
    pub fn with_fixtures(fixtures_dir: impl Into<PathBuf>) -> Self {
        Self::from_providers(vec![Box::new(FixtureCliProvider {
            fixtures_dir: fixtures_dir.into(),
        })])
    }

    /// Calls providers and publishes one assembled current state.
    pub fn probe(&self) -> Result<ProbeSnapshot> {
        let providers = self
            .providers
            .iter()
            .map(|provider| provider.as_ref())
            .collect::<Vec<_>>();
        Ok(ProbeSnapshot {
            generation: self.generation,
            current: assemble::probe(&providers),
        })
    }
}

/// Recorded command output used only by fixture tests.
struct FixtureCliProvider {
    fixtures_dir: PathBuf,
}

impl StateProvider for FixtureCliProvider {
    fn id(&self) -> &str {
        "linux-block"
    }

    fn responsibility(&self) -> &str {
        "fixture.linux-state"
    }

    fn backend(&self) -> ProviderBackend {
        ProviderBackend::Cli
    }

    fn probe(&self) -> std::result::Result<ProviderState, ProviderProbeError> {
        let lsblk = read_json::<LsblkOutput>(&self.fixtures_dir.join("lsblk.json"))?;
        let sysfs = read_optional_json::<SysfsOutput>(&self.fixtures_dir.join("sysfs.json"))?;
        let mounts = read_optional_json::<MountsOutput>(&self.fixtures_dir.join("findmnt.json"))?;
        Ok(assemble_linux_block_state(
            &lsblk,
            sysfs.as_ref(),
            mounts.as_ref(),
        ))
    }
}

/// Reads one required JSON fixture.
fn read_json<T>(path: &Path) -> std::result::Result<T, ProviderProbeError>
where
    T: serde::de::DeserializeOwned,
{
    let bytes = std::fs::read(path).map_err(|error| {
        ProviderProbeError::new(
            "fixture.read_failed",
            format!("{}: {error}", path.display()),
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        ProviderProbeError::new(
            "fixture.parse_failed",
            format!("{}: {error}", path.display()),
        )
    })
}

/// Reads one optional JSON fixture.
fn read_optional_json<T>(path: &Path) -> std::result::Result<Option<T>, ProviderProbeError>
where
    T: serde::de::DeserializeOwned,
{
    if !path.exists() {
        return Ok(None);
    }
    read_json(path).map(Some)
}

/// Converts the block-provider DTOs into one canonical provider contribution.
fn assemble_linux_block_state(
    lsblk: &LsblkOutput,
    sysfs: Option<&SysfsOutput>,
    mounts: Option<&MountsOutput>,
) -> ProviderState {
    let mut graph = NodeGraph::new();
    let mut mount_sources = HashMap::new();
    for device in &lsblk.blockdevices {
        add_block_device(device, None, 512, sysfs, &mut graph, &mut mount_sources);
    }

    ProviderState {
        graph,
        mounts: mounts.map_or_else(Vec::new, |output| {
            output
                .filesystems
                .iter()
                .map(|entry| ObservedMount {
                    source: mount_sources
                        .get(&entry.source)
                        .cloned()
                        .unwrap_or_else(|| external_mount_source(&entry.source, &entry.fstype)),
                    target: PathBuf::from(&entry.target),
                    options: entry
                        .options
                        .as_deref()
                        .map(|options| options.split(',').map(str::to_owned).collect())
                        .unwrap_or_default(),
                    context: MountContext::Host,
                })
                .collect()
        }),
        ..ProviderState::default()
    }
}

/// Adds one lsblk device, its structural edges, and its content node.
fn add_block_device(
    device: &LsblkDevice,
    parent: Option<NodeId>,
    parent_sector_size: u32,
    sysfs: Option<&SysfsOutput>,
    graph: &mut NodeGraph,
    mount_sources: &mut HashMap<String, MountSource>,
) -> NodeId {
    let path = device_path(device);
    let sysfs_device = sysfs.and_then(|output| {
        output
            .devices
            .iter()
            .find(|candidate| candidate.name == device.name)
    });
    let sector_size = parsed_u32(device.log_sectorsize.as_deref())
        .or_else(|| sysfs_device.map(|facts| facts.logical_block_size))
        .filter(|size| *size > 0)
        .unwrap_or(parent_sector_size);
    let node_id = stable_node_id("block", block_identity(device, sysfs_device));
    let offset = Bytes::new(
        device
            .start
            .unwrap_or(0)
            .saturating_mul(u64::from(parent_sector_size)),
    );
    let kind = match device.r#type.as_str() {
        "disk" => NodeKind::Disk,
        "part" => NonZeroU32::new(partition_number(&device.name)).map_or_else(
            || NodeKind::Unknown("partition without a number".into()),
            |number| NodeKind::Partition {
                number,
                offset,
                role: None,
                attributes: PartitionAttributes::default(),
            },
        ),
        "crypt" => NodeKind::DmCryptMapping {
            name: device.name.clone(),
        },
        other => NodeKind::Unknown(format!("lsblk:{other}")),
    };
    graph.insert_node(
        node_id,
        Node {
            kind: NodeSpec {
                kind,
                size: Some(Bytes::new(device.size)),
            },
            size: block_facts(device, sysfs_device, &path, sector_size),
        },
    );

    if let Some(parent) = parent {
        graph.insert_dependency(Dependency {
            from: parent,
            to: node_id,
            kind: if device.r#type == "part" {
                DependencyKind::Contains { offset }
            } else {
                DependencyKind::Backs
            },
        });
    }

    let children_parent = if device.r#type == "disk"
        && (device.pttype.is_some() || device.children.iter().any(|child| child.r#type == "part"))
    {
        let table_id = stable_node_id("partition-table", &path);
        let table = match device.pttype.as_deref() {
            Some("dos" | "mbr") => PartitionTable::Mbr,
            _ => PartitionTable::Gpt,
        };
        graph.insert_node(
            table_id,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::PartitionTable(table),
                    size: None,
                },
                size: NodeFacts {
                    presence: Presence::Present,
                    ..NodeFacts::default()
                },
            },
        );
        graph.insert_dependency(Dependency {
            from: node_id,
            to: table_id,
            kind: DependencyKind::Backs,
        });
        table_id
    } else {
        node_id
    };

    add_content(device, node_id, &path, graph, mount_sources);
    for child in &device.children {
        add_block_device(
            child,
            Some(children_parent),
            sector_size,
            sysfs,
            graph,
            mount_sources,
        );
    }
    node_id
}

/// Adds filesystem, swap, or provider marker content above a block node.
fn add_content(
    device: &LsblkDevice,
    block_id: NodeId,
    path: &str,
    graph: &mut NodeGraph,
    mount_sources: &mut HashMap<String, MountSource>,
) {
    let Some(fstype) = device.fstype.as_deref().filter(|value| !value.is_empty()) else {
        return;
    };
    let (kind, mountable) = match fstype {
        "swap" => (NodeKind::Swap, false),
        "LVM2_member" => (NodeKind::LvmPv, false),
        "crypto_LUKS" | "linux_raid_member" => {
            (NodeKind::Unknown(format!("signature:{fstype}")), false)
        }
        _ => (
            NodeKind::Filesystem {
                kind: filesystem_kind(fstype),
                label: device.label.clone().filter(|label| !label.is_empty()),
            },
            true,
        ),
    };
    let content_id = content_node_id(path);
    let identities = device
        .uuid
        .as_ref()
        .filter(|uuid| !uuid.is_empty())
        .map(|uuid| {
            vec![ExternalId::Filesystem {
                fs_type: fstype.to_owned(),
                value: uuid.clone(),
            }]
        })
        .unwrap_or_default();
    graph.insert_node(
        content_id,
        Node {
            kind: NodeSpec {
                kind,
                size: Some(Bytes::new(device.size)),
            },
            size: NodeFacts {
                presence: Presence::Present,
                identities,
                ..NodeFacts::default()
            },
        },
    );
    graph.insert_dependency(Dependency {
        from: block_id,
        to: content_id,
        kind: DependencyKind::Backs,
    });
    if mountable {
        mount_sources.insert(path.to_owned(), MountSource::Filesystem(content_id));
    }
}

/// Builds runtime facts for one block endpoint.
fn block_facts(
    device: &LsblkDevice,
    sysfs: Option<&crate::parse::sysfs::SysfsDevice>,
    path: &str,
    logical_size: u32,
) -> NodeFacts {
    let (major, minor) = sysfs
        .map(|facts| (facts.major, facts.minor))
        .or_else(|| parse_devno(device.maj_min.as_deref()))
        .unwrap_or_default();
    let physical_size = parsed_u32(device.phys_sectorsize.as_deref())
        .or_else(|| sysfs.map(|facts| facts.physical_block_size))
        .and_then(BlockSize::new);
    let mut identities = Vec::new();
    if let Some(wwn) = device.wwn.as_ref().filter(|wwn| !wwn.is_empty()) {
        identities.push(ExternalId::Wwn(wwn.clone()));
    }
    if let Some(serial) = sysfs.and_then(|facts| facts.serial.clone()) {
        identities.push(ExternalId::Serial {
            vendor: None,
            value: serial,
        });
    }
    if let Some(partuuid) = device
        .partuuid
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
    {
        identities.push(ExternalId::GptPartitionUuid(partuuid));
    }

    NodeFacts {
        presence: Presence::Present,
        identities,
        block: Some(BlockFacts {
            paths: vec![PathBuf::from(path)],
            devno: DeviceNumber { major, minor },
            geometry: BlockSize::new(logical_size).map(|logical_block_size| BlockGeometry {
                logical_block_size,
                physical_block_size: physical_size,
                alignment_offset: None,
                minimum_io_size: None,
                optimal_io_size: None,
            }),
            read_only: device
                .ro
                .as_deref()
                .and_then(|value| match value {
                    "0" => Some(false),
                    "1" => Some(true),
                    _ => None,
                })
                .or_else(|| sysfs.map(|facts| facts.read_only)),
        }),
        device: (device.r#type == "disk").then(|| DeviceInfo {
            model: sysfs.and_then(|facts| facts.model.clone()),
            vendor: None,
            transport: transport(&device.name),
            network_backing: None,
            rotational: sysfs.map(|facts| facts.rotational),
            removable: sysfs.map(|facts| facts.removable),
            zoned: None,
        }),
        ..NodeFacts::default()
    }
}

/// Maps a filesystem name without inventing provider-specific allocation data.
fn filesystem_kind(fstype: &str) -> FilesystemKind {
    match fstype {
        "ext2" => FilesystemKind::Ext2,
        "ext3" => FilesystemKind::Ext3,
        "ext4" => FilesystemKind::Ext4,
        "xfs" => FilesystemKind::Xfs,
        "btrfs" => FilesystemKind::Btrfs(BtrfsAllocation {
            data: HashSet::new(),
            metadata: HashSet::new(),
            system: HashSet::new(),
        }),
        "jfs" => FilesystemKind::Jfs,
        "reiserfs" => FilesystemKind::Reiserfs,
        "vfat" | "fat16" | "fat32" => FilesystemKind::Vfat,
        "ntfs" => FilesystemKind::Ntfs,
        "exfat" => FilesystemKind::Exfat,
        "f2fs" => FilesystemKind::F2fs,
        "ocfs2" => FilesystemKind::Ocfs2,
        "gfs2" => FilesystemKind::Gfs2,
        "apfs" => FilesystemKind::Apfs,
        other => FilesystemKind::Other(other.to_owned()),
    }
}

/// Converts a non-block mount source into its typed representation.
fn external_mount_source(source: &str, fstype: &str) -> MountSource {
    match fstype {
        "tmpfs" => MountSource::Tmpfs,
        "nfs" | "nfs4" => source.split_once(':').map_or_else(
            || MountSource::Other(source.to_owned()),
            |(server, export)| {
                MountSource::Network(NetworkMountSource::Nfs {
                    server: server.to_owned(),
                    export: export.to_owned(),
                })
            },
        ),
        "cifs" => source
            .strip_prefix("//")
            .and_then(|source| source.split_once('/'))
            .map_or_else(
                || MountSource::Other(source.to_owned()),
                |(server, share)| {
                    MountSource::Network(NetworkMountSource::Cifs {
                        server: server.to_owned(),
                        share: share.to_owned(),
                    })
                },
            ),
        _ => MountSource::Other(source.to_owned()),
    }
}

/// Returns a deterministic graph identity for provider-observed objects.
fn stable_node_id(kind: &str, identity: &str) -> NodeId {
    let value = format!("pdisks:{kind}:{identity}");
    let high = fnv1a(0xcbf2_9ce4_8422_2325, value.as_bytes());
    let low = fnv1a(0x8422_2325_cbf2_9ce4, value.as_bytes());
    NodeId::from_uuid(Uuid::from_u128((u128::from(high) << 64) | u128::from(low)))
}

/// Returns the shared cross-provider identity for a Linux block path.
fn block_node_id(path: &Path) -> NodeId {
    stable_node_id("block-path", &path.to_string_lossy())
}

/// Returns the shared cross-provider identity for content on a Linux block path.
fn content_node_id(path: &str) -> NodeId {
    stable_node_id("content-path", path)
}

/// Computes a stable non-cryptographic identity component for provider keys.
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Chooses the strongest identity available to the block provider.
fn block_identity<'a>(
    device: &'a LsblkDevice,
    sysfs: Option<&'a crate::parse::sysfs::SysfsDevice>,
) -> &'a str {
    device
        .wwn
        .as_deref()
        .filter(|value| !value.is_empty())
        .or_else(|| sysfs.and_then(|facts| facts.serial.as_deref()))
        .or_else(|| device.partuuid.as_deref().filter(|value| !value.is_empty()))
        .unwrap_or(&device.name)
}

/// Returns the observed device path, falling back to the conventional `/dev` path.
fn device_path(device: &LsblkDevice) -> String {
    device
        .path
        .clone()
        .unwrap_or_else(|| format!("/dev/{}", device.name))
}

/// Extracts the trailing partition number from a Linux device name.
fn partition_number(name: &str) -> u32 {
    name.chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

/// Parses a `major:minor` device number.
fn parse_devno(value: Option<&str>) -> Option<(u32, u32)> {
    let (major, minor) = value?.split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Parses an optional decimal u32 field.
fn parsed_u32(value: Option<&str>) -> Option<u32> {
    value?.parse().ok()
}

/// Infers a coarse transport from the Linux kernel device name.
fn transport(name: &str) -> Option<Transport> {
    if name.starts_with("nvme") {
        Some(Transport::Nvme)
    } else if name.starts_with("sd") {
        Some(Transport::Scsi)
    } else if name.starts_with("vd") {
        Some(Transport::Virtio)
    } else if name.starts_with("mmcblk") {
        Some(Transport::Mmc)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage_provider::{
        BlkidEntry, BtrfsEntry, BtrfsSubvolumeEntry, DevmapperEntry, FdiskPartition, FdiskTable,
        LvmEntry, LvmLvEntry, LvmPvEntry, LvmVgEntry, MdraidArrayEntry, NativeDeviceNumber,
        NvmeControllerEntry, NvmeEntry, NvmeNamespaceEntry, NvmeSubsystemEntry, ZfsDatasetEntry,
        ZfsDatasetKind, ZfsEntry,
    };

    /// Ensures native partial observations become one connected canonical graph.
    #[test]
    fn native_block_observations_form_connected_graph() {
        let disk = UdevBlockEntry {
            devnode: PathBuf::from("/dev/vda"),
            sysname: "vda".to_owned(),
            devtype: Some("disk".to_owned()),
            major: 252,
            minor: 0,
            size_sectors: Some(2_000),
            logical_block_size: Some(512),
            physical_block_size: Some(512),
            model: Some("fixture".to_owned()),
            serial: None,
            wwn: None,
            read_only: Some(false),
            rotational: Some(false),
            removable: Some(false),
        };
        let mut partition = disk.clone();
        partition.devnode = PathBuf::from("/dev/vda1");
        partition.sysname = "vda1".to_owned();
        partition.devtype = Some("partition".to_owned());
        partition.minor = 1;
        partition.size_sectors = Some(1_000);
        let graph = assemble_native_block_state(
            &[disk, partition],
            &[FdiskTable {
                device: PathBuf::from("/dev/vda"),
                label: Some("gpt".to_owned()),
                sector_size: 512,
                partitions: vec![FdiskPartition {
                    number: 1,
                    start_sectors: 2,
                    size_sectors: 1_000,
                    partition_type: None,
                    name: None,
                    uuid: None,
                    bootable: false,
                }],
            }],
            &[BlkidEntry {
                device: PathBuf::from("/dev/vda1"),
                signature_type: Some("ext4".to_owned()),
                uuid: Some("fixture-fs".to_owned()),
                label: Some("root".to_owned()),
                partition_uuid: None,
            }],
        )
        .graph;

        assert_eq!(graph.nodes().len(), 4);
        assert_eq!(graph.dependencies().len(), 3);
        assert!(matches!(
            graph
                .node(&content_node_id("/dev/vda1"))
                .map(|node| &node.kind.kind),
            Some(NodeKind::Filesystem { .. })
        ));
    }

    /// Ensures LVM identities join PV, VG, and active mapper-backed LV nodes.
    #[test]
    fn lvm_observations_enrich_existing_block_graph() {
        let mapper_endpoint = UdevBlockEntry {
            devnode: PathBuf::from("/dev/dm-0"),
            sysname: "dm-0".to_owned(),
            devtype: Some("disk".to_owned()),
            major: 253,
            minor: 0,
            size_sectors: Some(1_000),
            logical_block_size: Some(512),
            physical_block_size: Some(512),
            model: None,
            serial: None,
            wwn: None,
            read_only: Some(false),
            rotational: None,
            removable: None,
        };
        let mut graph = NodeGraph::new();
        insert_native_endpoint(
            &mut graph,
            &mapper_endpoint,
            NodeKind::Unknown("device-mapper:linear".to_owned()),
        );
        graph.insert_node(
            content_node_id("/dev/vda1"),
            Node {
                kind: NodeSpec {
                    kind: NodeKind::LvmPv,
                    size: Some(Bytes::new(512_000)),
                },
                size: NodeFacts {
                    presence: Presence::Present,
                    ..NodeFacts::default()
                },
            },
        );
        let mappings = vec![DevmapperEntry {
            name: "vg-root".to_owned(),
            uuid: Some("LVM-aaaaaaaa-bbbbbbbb".to_owned()),
            devno: NativeDeviceNumber {
                major: 253,
                minor: 0,
            },
            targets: vec!["linear".to_owned()],
            dependencies: Vec::new(),
            read_only: false,
        }];
        let topology = LvmEntry {
            pvs: vec![LvmPvEntry {
                device: PathBuf::from("/dev/vda1"),
                uuid: "pv-uuid".to_owned(),
                vg_name: Some("vg".to_owned()),
                vg_uuid: Some("aaaaaaaa".to_owned()),
                size: 512_000,
            }],
            vgs: vec![LvmVgEntry {
                name: "vg".to_owned(),
                uuid: "aaaaaaaa".to_owned(),
                size: 512_000,
                extent_size: 4_194_304,
            }],
            lvs: vec![LvmLvEntry {
                name: "root".to_owned(),
                vg_name: "vg".to_owned(),
                uuid: "bbbbbbbb".to_owned(),
                size: 512_000,
                segment_type: Some("linear".to_owned()),
            }],
        };

        enrich_lvm(&mut graph, &[mapper_endpoint], &mappings, &topology);

        let vg_id = stable_node_id("lvm-vg", "aaaaaaaa");
        let lv_id = block_node_id(Path::new("/dev/dm-0"));
        assert!(matches!(
            graph.node(&vg_id).map(|node| &node.kind.kind),
            Some(NodeKind::LvmVg { .. })
        ));
        assert!(matches!(
            graph.node(&lv_id).map(|node| &node.kind.kind),
            Some(NodeKind::LvmLv(_))
        ));
        assert!(graph.dependencies().any(|dependency| {
            dependency.from == vg_id
                && dependency.to == lv_id
                && dependency.kind == DependencyKind::Provides
        }));
    }

    /// Ensures MD levels requiring unavailable layout data stay unclassified.
    #[test]
    fn mdraid_does_not_invent_missing_layout() {
        let mut array = MdraidArrayEntry {
            device: PathBuf::from("/dev/md0"),
            level: "raid1".to_owned(),
            metadata: Some("1.2".to_owned()),
            uuid: None,
            size: 1_024,
            raid_devices: 2,
        };

        assert!(matches!(mdraid_kind(&array, &[]), NodeKind::MdArray(_)));
        array.level = "raid5".to_owned();
        assert_eq!(
            mdraid_kind(&array, &[]),
            NodeKind::Unknown("md:raid5".to_owned())
        );
    }

    /// Ensures the remaining native topologies map into canonical graph nodes.
    #[test]
    fn extended_native_topologies_enrich_the_graph() {
        let mut graph = NodeGraph::new();
        let filesystem_id = content_node_id("/dev/vda1");
        graph.insert_node(
            filesystem_id,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::Filesystem {
                        kind: FilesystemKind::Btrfs(BtrfsAllocation {
                            data: HashSet::new(),
                            metadata: HashSet::new(),
                            system: HashSet::new(),
                        }),
                        label: None,
                    },
                    size: None,
                },
                size: NodeFacts::default(),
            },
        );
        enrich_btrfs(
            &mut graph,
            &[ObservedMount {
                source: MountSource::Filesystem(filesystem_id),
                target: PathBuf::from("/mnt/test"),
                options: Vec::new(),
                context: MountContext::Host,
            }],
            &[BtrfsEntry {
                mountpoint: PathBuf::from("/mnt/test"),
                subvolumes: vec![BtrfsSubvolumeEntry {
                    id: 256,
                    path: PathBuf::from("home"),
                    read_only: false,
                    is_default: true,
                }],
            }],
        );
        enrich_zfs(
            &mut graph,
            &ZfsEntry {
                pools: vec!["tank".to_owned()],
                datasets: vec![ZfsDatasetEntry {
                    name: "tank/data".to_owned(),
                    kind: ZfsDatasetKind::Filesystem,
                }],
            },
        );
        enrich_nvme(
            &mut graph,
            &NvmeEntry {
                subsystems: vec![NvmeSubsystemEntry {
                    name: "nvme-subsys0".to_owned(),
                    nqn: "nqn.test".to_owned(),
                    controllers: vec![NvmeControllerEntry {
                        name: "nvme0".to_owned(),
                        transport: Some("pcie".to_owned()),
                    }],
                    namespaces: vec![NvmeNamespaceEntry {
                        name: "nvme0n1".to_owned(),
                        nsid: 1,
                        lba_size: 512,
                        metadata_size: 0,
                        nguid: Some([1; 16]),
                        eui64: None,
                        uuid: None,
                    }],
                }],
            },
        );

        assert!(
            graph
                .nodes()
                .any(|(_, node)| matches!(node.kind.kind, NodeKind::BtrfsSubvolume { .. }))
        );
        assert!(
            graph
                .nodes()
                .any(|(_, node)| matches!(node.kind.kind, NodeKind::ZfsDataset { .. }))
        );
        assert!(matches!(
            graph
                .node(&block_node_id(Path::new("/dev/nvme0n1")))
                .map(|node| &node.kind.kind),
            Some(NodeKind::NvmeNamespace(_))
        ));
        assert!(
            graph
                .relations()
                .any(|relation| relation.kind == RelationKind::NvmeAttachedTo)
        );
    }
}
