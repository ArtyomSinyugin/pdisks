//! Compatibility checks between provider-shaped facts and the canonical model.

use std::{
    collections::HashSet,
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use uuid::Uuid;

use super::*;

/// Verifies lossless JSON serialization for a model value.
fn roundtrip<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value)
        .unwrap_or_else(|error| panic!("model value must serialize: {error}"));
    let decoded: T = serde_json::from_str(&json)
        .unwrap_or_else(|error| panic!("model value must deserialize: {error}"));
    assert_eq!(&decoded, value);
}

/// Creates a deterministic node ID for fixtures.
fn id(value: u128) -> NodeId {
    NodeId::from_uuid(Uuid::from_u128(value))
}

/// Creates a canonical node from provider-shaped fixture facts.
fn node(kind: NodeKind, size: Option<u64>, facts: NodeFacts) -> Node {
    Node {
        kind: NodeSpec {
            kind,
            size: size.map(Bytes::new),
        },
        size: facts,
    }
}

#[test]
/// Verifies the common block-provider stack.
fn lsblk_blkid_libfdisk_and_udev_fit_existing_block_types() {
    let disk_id = id(1);
    let table_id = id(2);
    let partition_id = id(3);
    let filesystem_id = id(4);
    let mut graph = NodeGraph::new();

    graph.insert_node(
        disk_id,
        node(
            NodeKind::Disk,
            Some(1_000_000_000),
            NodeFacts {
                observed_in: Some(ProbeEpoch::new(3)),
                presence: Presence::Present,
                identities: vec![
                    ExternalId::Wwn("0x5000c500abcd".into()),
                    ExternalId::Serial {
                        vendor: Some("ATA".into()),
                        value: "SERIAL".into(),
                    },
                ],
                block: Some(BlockFacts {
                    paths: vec![
                        PathBuf::from("/dev/sda"),
                        PathBuf::from("/dev/disk/by-id/wwn-test"),
                    ],
                    devno: DeviceNumber { major: 8, minor: 0 },
                    geometry: Some(BlockGeometry {
                        logical_block_size: BlockSize::new(512).unwrap_or_else(|| unreachable!()),
                        physical_block_size: BlockSize::new(4096),
                        alignment_offset: Some(Bytes::new(0)),
                        minimum_io_size: Some(Bytes::new(4096)),
                        optimal_io_size: Some(Bytes::new(1_048_576)),
                    }),
                    read_only: Some(false),
                }),
                device: Some(DeviceInfo {
                    model: Some("TEST DISK".into()),
                    vendor: Some("ATA".into()),
                    transport: Some(Transport::Sata),
                    network_backing: None,
                    rotational: Some(false),
                    removable: Some(false),
                    zoned: Some(ZonedModel::None),
                }),
            },
        ),
    );
    graph.insert_node(
        table_id,
        node(
            NodeKind::PartitionTable(PartitionTable::Gpt),
            None,
            NodeFacts::default(),
        ),
    );
    graph.insert_node(
        partition_id,
        node(
            NodeKind::Partition {
                number: NonZeroU32::new(1).unwrap_or_else(|| unreachable!()),
                offset: Bytes::new(1_048_576),
                role: Some(UsageRole::Root),
                attributes: PartitionAttributes {
                    gpt: Some(1 << 60),
                    bootable: None,
                },
            },
            Some(900_000_000),
            NodeFacts {
                identities: vec![ExternalId::GptPartitionUuid(Uuid::from_u128(30))],
                presence: Presence::Present,
                ..NodeFacts::default()
            },
        ),
    );
    graph.insert_node(
        filesystem_id,
        node(
            NodeKind::Filesystem {
                kind: FilesystemKind::Ext4,
                label: Some("root".into()),
            },
            Some(890_000_000),
            NodeFacts {
                identities: vec![ExternalId::Filesystem {
                    fs_type: "ext4".into(),
                    value: Uuid::from_u128(40).to_string(),
                }],
                presence: Presence::Present,
                ..NodeFacts::default()
            },
        ),
    );

    assert!(graph.insert_dependency(Dependency {
        from: disk_id,
        to: table_id,
        kind: DependencyKind::Backs,
    }));
    assert!(graph.insert_dependency(Dependency {
        from: table_id,
        to: partition_id,
        kind: DependencyKind::Contains {
            offset: Bytes::new(1_048_576),
        },
    }));
    assert!(graph.insert_dependency(Dependency {
        from: partition_id,
        to: filesystem_id,
        kind: DependencyKind::Backs,
    }));

    assert_eq!(graph.nodes().len(), 4);
    assert_eq!(graph.dependencies().len(), 3);
    roundtrip(&graph);
}

#[test]
/// Verifies NVMe namespace facts and identities.
fn libnvme_and_nvme_cli_fit_existing_nvme_types() {
    let namespace = NvmeNamespace {
        nsid: NvmeNamespaceId::new(1).unwrap_or_else(|| unreachable!()),
        format: NvmeLbaFormat {
            data_size: BlockSize::new(4096).unwrap_or_else(|| unreachable!()),
            metadata_size: 16,
        },
    };
    let node = node(
        NodeKind::NvmeNamespace(namespace),
        Some(2_000_000_000),
        NodeFacts {
            identities: vec![
                ExternalId::NvmeNguid([1; 16]),
                ExternalId::NvmeEui64([2; 8]),
                ExternalId::NvmeUuid(Uuid::from_u128(5)),
            ],
            presence: Presence::Present,
            ..NodeFacts::default()
        },
    );
    roundtrip(&node);
}

#[test]
/// Verifies mdraid topology and membership facts.
fn mdadm_and_sysfs_md_fit_existing_raid_types() {
    let array = MdArray {
        personality: MdPersonality::Raid(MdRaid::Raid6 {
            layout: MdRaid6Layout::Standard(MdParityLayout::Rotating(
                ParityRotation::LeftSymmetric,
            )),
            chunk_size: Bytes::new(524_288),
        }),
        metadata: MdMetadata::Native(MdNativeMetadata::V1_2),
    };
    let member = MemberRole::Md(MdMember {
        role: MdMemberRole::Data,
        slot: Some(0),
        data_offset: Some(Bytes::new(1_048_576)),
    });
    roundtrip(&array);
    roundtrip(&member);
    roundtrip(&[
        MdPersonality::Raid(MdRaid::Raid0 {
            layout: MdRaid0Layout::Original,
            chunk_size: Bytes::new(64 * 1024),
        }),
        MdPersonality::Raid(MdRaid::Raid0 {
            layout: MdRaid0Layout::Alternate,
            chunk_size: Bytes::new(64 * 1024),
        }),
        MdPersonality::Raid(MdRaid::Raid0 {
            layout: MdRaid0Layout::Unspecified,
            chunk_size: Bytes::new(64 * 1024),
        }),
        MdPersonality::Raid(MdRaid::Raid1),
        MdPersonality::Raid(MdRaid::Raid4 {
            chunk_size: Bytes::new(64 * 1024),
        }),
        MdPersonality::Raid(MdRaid::Raid5 {
            layout: MdParityLayout::ParityFirst,
            chunk_size: Bytes::new(64 * 1024),
        }),
        MdPersonality::Raid(MdRaid::Raid5 {
            layout: MdParityLayout::ParityLast,
            chunk_size: Bytes::new(64 * 1024),
        }),
        MdPersonality::Raid(MdRaid::Raid6 {
            layout: MdRaid6Layout::DedicatedQ(MdRaid6DedicatedQ::ParityFirst6),
            chunk_size: Bytes::new(64 * 1024),
        }),
        MdPersonality::Raid(MdRaid::Raid10 {
            layout: MdRaid10Layout {
                mode: MdRaid10Mode::Far,
                copies: CopyCount::new(2).unwrap_or_else(|| unreachable!()),
            },
            chunk_size: Bytes::new(64 * 1024),
        }),
        MdPersonality::Linear {
            rounding: Some(Bytes::new(64 * 1024)),
        },
        MdPersonality::Multipath,
        MdPersonality::Faulty,
        MdPersonality::Container,
    ]);
    roundtrip(&[
        MdMetadata::None,
        MdMetadata::Native(MdNativeMetadata::V0_90),
        MdMetadata::Native(MdNativeMetadata::V1_0),
        MdMetadata::Native(MdNativeMetadata::V1_1),
        MdMetadata::Native(MdNativeMetadata::V1_2),
        MdMetadata::External(MdExternalMetadata::Ddf),
        MdMetadata::External(MdExternalMetadata::Imsm),
    ]);
    roundtrip(&[
        MdParityLayout::Rotating(ParityRotation::LeftSymmetric),
        MdParityLayout::Rotating(ParityRotation::LeftAsymmetric),
        MdParityLayout::Rotating(ParityRotation::RightSymmetric),
        MdParityLayout::Rotating(ParityRotation::RightAsymmetric),
        MdParityLayout::ParityFirst,
        MdParityLayout::ParityLast,
        MdParityLayout::DdfZeroRestart,
        MdParityLayout::DdfNRestart,
        MdParityLayout::DdfNContinue,
    ]);
    roundtrip(&[
        MdRaid6DedicatedQ::LeftSymmetric6,
        MdRaid6DedicatedQ::RightSymmetric6,
        MdRaid6DedicatedQ::LeftAsymmetric6,
        MdRaid6DedicatedQ::RightAsymmetric6,
        MdRaid6DedicatedQ::ParityFirst6,
    ]);
    roundtrip(&[MdRaid10Mode::Near, MdRaid10Mode::Offset, MdRaid10Mode::Far]);
}

#[test]
/// Verifies LVM logical-volume layouts.
fn lvm_json_reports_fit_all_existing_lv_layouts() {
    let stripes = DataStripeCount::new(3).unwrap_or_else(|| unreachable!());
    let copies = CopyCount::new(2).unwrap_or_else(|| unreachable!());
    let kinds = [
        LvmLvKind::Linear,
        LvmLvKind::Striped { stripes },
        LvmLvKind::Raid(LvmRaid::Raid5 {
            layout: LvmRaid5Layout::Rotating(ParityRotation::RightAsymmetric),
            data_width: stripes,
            stripe_size: Bytes::new(65_536),
        }),
        LvmLvKind::Mirror { copies },
        LvmLvKind::ThinPool,
        LvmLvKind::Thin,
        LvmLvKind::Snapshot,
        LvmLvKind::CachePool,
        LvmLvKind::Cache,
        LvmLvKind::WriteCache,
        LvmLvKind::Other("vdo".into()),
    ];
    for (index, kind) in kinds.into_iter().enumerate() {
        roundtrip(&LvmLv {
            name: format!("lv{index}"),
            kind,
        });
    }
    roundtrip(&[
        LvmRaid::Raid0 {
            variant: LvmRaid0Variant::Raid0,
            data_width: stripes,
            stripe_size: Bytes::new(64 * 1024),
        },
        LvmRaid::Raid0 {
            variant: LvmRaid0Variant::Raid0Meta,
            data_width: stripes,
            stripe_size: Bytes::new(64 * 1024),
        },
        LvmRaid::Raid1 { copies },
        LvmRaid::Raid4 {
            data_width: stripes,
            stripe_size: Bytes::new(64 * 1024),
        },
        LvmRaid::Raid5 {
            layout: LvmRaid5Layout::ParityN,
            data_width: stripes,
            stripe_size: Bytes::new(64 * 1024),
        },
        LvmRaid::Raid6 {
            layout: LvmRaid6Layout::DedicatedParity,
            data_width: stripes,
            stripe_size: Bytes::new(64 * 1024),
        },
        LvmRaid::Raid10 {
            copies,
            data_width: stripes,
            stripe_size: Bytes::new(64 * 1024),
        },
    ]);
    roundtrip(&[
        LvmRaid5Layout::Rotating(ParityRotation::LeftSymmetric),
        LvmRaid5Layout::Rotating(ParityRotation::LeftAsymmetric),
        LvmRaid5Layout::Rotating(ParityRotation::RightSymmetric),
        LvmRaid5Layout::Rotating(ParityRotation::RightAsymmetric),
        LvmRaid5Layout::ParityN,
    ]);
    roundtrip(&[
        LvmRaid6Layout::ZeroRestart,
        LvmRaid6Layout::NRestart,
        LvmRaid6Layout::NContinue,
        LvmRaid6Layout::DedicatedParity,
        LvmRaid6Layout::Raid5Compatible(ParityRotation::LeftSymmetric),
        LvmRaid6Layout::Raid5Compatible(ParityRotation::LeftAsymmetric),
        LvmRaid6Layout::Raid5Compatible(ParityRotation::RightSymmetric),
        LvmRaid6Layout::Raid5Compatible(ParityRotation::RightAsymmetric),
    ]);
}

#[test]
/// Verifies cryptsetup and device-mapper node kinds.
fn cryptsetup_and_device_mapper_fit_existing_crypto_and_cache_types() {
    let kinds = [
        NodeKind::LuksContainer {
            version: LuksVersion::Luks1,
        },
        NodeKind::LuksContainer {
            version: LuksVersion::Luks2,
        },
        NodeKind::DmCryptMapping {
            name: "cryptroot".into(),
        },
        NodeKind::DmIntegrity {
            name: "integrity".into(),
        },
        NodeKind::DmVerity {
            name: "verity".into(),
        },
        NodeKind::Bcache {
            name: "bcache0".into(),
            mode: BcacheMode::Writeback,
        },
        NodeKind::DmCache {
            name: "cached".into(),
            mode: DmCacheMode::Writethrough,
        },
        NodeKind::DmWritecache {
            name: "writecache".into(),
        },
    ];
    roundtrip(&kinds);
    roundtrip(&[
        BcacheMode::Writeback,
        BcacheMode::Writethrough,
        BcacheMode::Writearound,
        BcacheMode::None,
    ]);
    roundtrip(&[
        DmCacheMode::Writeback,
        DmCacheMode::Writethrough,
        DmCacheMode::Passthrough,
    ]);
}

#[test]
/// Verifies Btrfs profiles, members, and subvolumes.
fn btrfs_progs_fit_existing_profiles_members_and_subvolumes() {
    let allocation = BtrfsAllocation {
        data: HashSet::from([
            BtrfsProfile::Single,
            BtrfsProfile::Dup,
            BtrfsProfile::Raid0,
            BtrfsProfile::Raid1,
            BtrfsProfile::Raid1C3,
            BtrfsProfile::Raid1C4,
            BtrfsProfile::Raid10,
            BtrfsProfile::Raid5,
            BtrfsProfile::Raid6,
        ]),
        metadata: HashSet::from([BtrfsProfile::Raid1C3]),
        system: HashSet::from([BtrfsProfile::Dup]),
    };
    let filesystem = NodeKind::Filesystem {
        kind: FilesystemKind::Btrfs(allocation),
        label: Some("pool".into()),
    };
    let member = BtrfsMember {
        devid: NonZeroU64::new(2),
    };
    let subvolume = NodeKind::BtrfsSubvolume {
        name: "@home".into(),
        read_only: true,
        is_default: false,
    };
    roundtrip(&(filesystem, member, subvolume));
}

#[test]
/// Verifies ZFS pool, vdev, dataset, and volume facts.
fn zpool_and_zfs_fit_existing_pool_vdev_dataset_and_zvol_types() {
    let stripes = DataStripeCount::new(8).unwrap_or_else(|| unreachable!());
    let kinds = [
        NodeKind::ZfsPool {
            name: "tank".into(),
        },
        NodeKind::ZfsVdev(ZfsVdevKind::Leaf),
        NodeKind::ZfsVdev(ZfsVdevKind::Mirror),
        NodeKind::ZfsVdev(ZfsVdevKind::RaidZ {
            parity: ZfsParity::One,
        }),
        NodeKind::ZfsVdev(ZfsVdevKind::RaidZ {
            parity: ZfsParity::Three,
        }),
        NodeKind::ZfsVdev(ZfsVdevKind::DRaid {
            parity: ZfsParity::Two,
            data_width: stripes,
            distributed_spares: 1,
        }),
        NodeKind::ZfsDataset {
            name: "tank/home".into(),
        },
        NodeKind::ZfsVolume {
            name: "tank/vm".into(),
        },
        NodeKind::ZfsVdev(ZfsVdevKind::Other("replacing".into())),
    ];
    let classes = [
        ZfsVdevClass::Data,
        ZfsVdevClass::Log,
        ZfsVdevClass::Special,
        ZfsVdevClass::Dedup,
        ZfsVdevClass::Cache,
        ZfsVdevClass::Spare,
    ];
    roundtrip(&(kinds, classes));
}

#[test]
/// Verifies cross-provider graph relation kinds.
fn graph_relations_cover_multipath_nvme_snapshots_and_replication() {
    let dependencies = [
        DependencyKind::Backs,
        DependencyKind::Contains {
            offset: Bytes::new(4096),
        },
        DependencyKind::Provides,
        DependencyKind::MemberOf(MemberRole::LvmPv),
        DependencyKind::Caches(CacheRole::Metadata),
        DependencyKind::Path,
    ];
    let relations = [
        RelationKind::SnapshotOf,
        RelationKind::NvmeAttachedTo,
        RelationKind::Replicates,
    ];
    roundtrip(&(dependencies, relations));
}

#[test]
/// Verifies stable names for every filesystem kind.
fn every_existing_filesystem_kind_has_a_provider_name() {
    let btrfs = BtrfsAllocation {
        data: HashSet::new(),
        metadata: HashSet::new(),
        system: HashSet::new(),
    };
    let kinds = [
        FilesystemKind::Ext2,
        FilesystemKind::Ext3,
        FilesystemKind::Ext4,
        FilesystemKind::Xfs,
        FilesystemKind::Btrfs(btrfs),
        FilesystemKind::Jfs,
        FilesystemKind::Reiserfs,
        FilesystemKind::Vfat,
        FilesystemKind::Ntfs,
        FilesystemKind::Exfat,
        FilesystemKind::F2fs,
        FilesystemKind::Ocfs2,
        FilesystemKind::Gfs2,
        FilesystemKind::Apfs,
        FilesystemKind::Other("bcachefs".into()),
    ];
    let expected = [
        "ext2", "ext3", "ext4", "xfs", "btrfs", "jfs", "reiserfs", "vfat", "ntfs", "exfat", "f2fs",
        "ocfs2", "gfs2", "apfs", "bcachefs",
    ];
    for (kind, expected) in kinds.iter().zip(expected) {
        assert_eq!(kind.as_str(), expected);
    }
}

#[test]
/// Verifies rejection of impossible numeric provider values.
fn provider_numeric_boundaries_reject_impossible_zero_values() {
    assert!(BlockSize::new(0).is_none());
    assert!(NvmeNamespaceId::new(0).is_none());
    assert!(DataStripeCount::new(0).is_none());
    assert!(CopyCount::new(0).is_none());
    assert!(CopyCount::new(1).is_none());
}

#[test]
/// Verifies network identities, transports, and zoned media.
fn network_and_zoned_providers_preserve_identity_and_transport() {
    let identities = [
        ExternalId::Iscsi {
            target_iqn: "iqn.2026-09.example:disk".into(),
            lun: 7,
        },
        ExternalId::NvmeOf {
            subsystem_nqn: "nqn.2026-09.example:nvme".into(),
            namespace: NonZeroU32::new(1).unwrap_or_else(|| unreachable!()),
        },
    ];
    let transports = [Transport::NvmeOf, Transport::Iscsi];
    roundtrip(&(identities, transports, ZonedModel::HostManaged));
}

#[test]
/// Classifies observed remote block devices without connection-management state.
fn network_storage_is_derived_from_transport() {
    for transport in [Transport::NvmeOf, Transport::FibreChannel, Transport::Iscsi] {
        let device = DeviceInfo {
            transport: Some(transport),
            ..DeviceInfo::default()
        };
        assert_eq!(device.is_network(), Some(true));
    }

    for transport in [
        Transport::Nvme,
        Transport::Sata,
        Transport::Sas,
        Transport::Scsi,
        Transport::Usb,
        Transport::Virtio,
        Transport::Mmc,
    ] {
        let device = DeviceInfo {
            transport: Some(transport),
            ..DeviceInfo::default()
        };
        assert_eq!(device.is_network(), Some(false));
    }

    assert_eq!(DeviceInfo::default().is_network(), None);

    for backing in [NetworkBacking::Rbd, NetworkBacking::Nbd] {
        let device = DeviceInfo {
            network_backing: Some(backing),
            ..DeviceInfo::default()
        };
        assert_eq!(device.is_network(), Some(true));
        assert_eq!(device.transport, None);
        roundtrip(&device);
    }
}

#[test]
/// Verifies every canonical external identity emitted by current providers.
fn every_external_identity_variant_roundtrips() {
    roundtrip(&[
        ExternalId::Wwn("0x5000000000000001".into()),
        ExternalId::Serial {
            vendor: Some("fixture".into()),
            value: "serial-1".into(),
        },
        ExternalId::GptPartitionUuid(Uuid::from_u128(1)),
        ExternalId::MbrPartitionId {
            disk_signature: 0x1234_5678,
            number: NonZeroU32::new(1).unwrap_or_else(|| unreachable!()),
        },
        ExternalId::LuksUuid(Uuid::from_u128(2)),
        ExternalId::MdUuid("md-uuid".into()),
        ExternalId::LvmPvUuid("pv-uuid".into()),
        ExternalId::LvmVgUuid("vg-uuid".into()),
        ExternalId::LvmLvUuid("lv-uuid".into()),
        ExternalId::Filesystem {
            fs_type: "ext4".into(),
            value: "filesystem-uuid".into(),
        },
        ExternalId::BtrfsFsid(Uuid::from_u128(3)),
        ExternalId::BtrfsSubvolumeUuid(Uuid::from_u128(4)),
        ExternalId::ZfsGuid(5),
        ExternalId::NvmeNguid([6; 16]),
        ExternalId::NvmeEui64([7; 8]),
        ExternalId::NvmeUuid(Uuid::from_u128(8)),
        ExternalId::Iscsi {
            target_iqn: "iqn.2026-09.example:fixture".into(),
            lun: 9,
        },
        ExternalId::NvmeOf {
            subsystem_nqn: "nqn.2026-09.example:fixture".into(),
            namespace: NonZeroU32::new(10).unwrap_or_else(|| unreachable!()),
        },
    ]);
}

#[test]
/// Builds the canonical graph from the sanitized real `lsblk -J -b -O` shape.
fn real_lsblk_nvme_shape_builds_existing_model_without_partition_type() {
    let source: Value = serde_json::from_str(include_str!("fixtures/lsblk_nvme.json"))
        .unwrap_or_else(|error| panic!("lsblk fixture parses: {error}"));
    let disk = &source["blockdevices"][0];
    let disk_id = id(1_000);
    let table_id = id(1_001);
    let mut graph = NodeGraph::new();

    graph.insert_node(
        disk_id,
        node(
            NodeKind::Disk,
            disk["size"].as_u64(),
            NodeFacts {
                presence: Presence::Present,
                identities: vec![
                    ExternalId::Wwn(
                        disk["wwn"]
                            .as_str()
                            .unwrap_or_else(|| unreachable!())
                            .into(),
                    ),
                    ExternalId::Serial {
                        vendor: None,
                        value: disk["serial"]
                            .as_str()
                            .unwrap_or_else(|| unreachable!())
                            .into(),
                    },
                ],
                block: Some(BlockFacts {
                    paths: vec![PathBuf::from(
                        disk["path"].as_str().unwrap_or_else(|| unreachable!()),
                    )],
                    devno: DeviceNumber {
                        major: 259,
                        minor: 0,
                    },
                    geometry: Some(BlockGeometry {
                        logical_block_size: BlockSize::new(512).unwrap_or_else(|| unreachable!()),
                        physical_block_size: BlockSize::new(512),
                        alignment_offset: None,
                        minimum_io_size: None,
                        optimal_io_size: None,
                    }),
                    read_only: disk["ro"].as_bool(),
                }),
                device: Some(DeviceInfo {
                    model: disk["model"].as_str().map(str::to_owned),
                    vendor: None,
                    transport: Some(Transport::Nvme),
                    network_backing: None,
                    rotational: disk["rota"].as_bool(),
                    removable: disk["rm"].as_bool(),
                    zoned: Some(ZonedModel::None),
                }),
                ..NodeFacts::default()
            },
        ),
    );
    graph.insert_node(
        table_id,
        node(
            NodeKind::PartitionTable(PartitionTable::Gpt),
            None,
            NodeFacts::default(),
        ),
    );
    assert!(graph.insert_dependency(Dependency {
        from: disk_id,
        to: table_id,
        kind: DependencyKind::Backs,
    }));

    for (index, partition) in disk["children"]
        .as_array()
        .unwrap_or_else(|| unreachable!())
        .iter()
        .enumerate()
    {
        let partition_id = id(1_010 + index as u128);
        let content_id = id(1_020 + index as u128);
        let mounts = partition["mountpoints"]
            .as_array()
            .unwrap_or_else(|| unreachable!());
        let role = if mounts.iter().any(|mount| mount == "/boot/efi") {
            Some(UsageRole::Boot)
        } else if mounts.iter().any(|mount| mount == "/") {
            Some(UsageRole::Root)
        } else if mounts.iter().any(|mount| mount == "/home") {
            Some(UsageRole::Home)
        } else {
            None
        };
        let number = u32::try_from(
            partition["partn"]
                .as_u64()
                .unwrap_or_else(|| unreachable!()),
        )
        .unwrap_or_else(|error| panic!("partition number fits u32: {error}"));
        let partition_uuid = Uuid::parse_str(
            partition["partuuid"]
                .as_str()
                .unwrap_or_else(|| unreachable!()),
        )
        .unwrap_or_else(|error| panic!("partition UUID parses: {error}"));

        graph.insert_node(
            partition_id,
            node(
                NodeKind::Partition {
                    number: NonZeroU32::new(number).unwrap_or_else(|| unreachable!()),
                    offset: Bytes::new(
                        partition["start"]
                            .as_u64()
                            .unwrap_or_else(|| unreachable!())
                            * 512,
                    ),
                    role,
                    attributes: PartitionAttributes::default(),
                },
                partition["size"].as_u64(),
                NodeFacts {
                    presence: Presence::Present,
                    identities: vec![ExternalId::GptPartitionUuid(partition_uuid)],
                    ..NodeFacts::default()
                },
            ),
        );
        assert!(
            graph.insert_dependency(Dependency {
                from: table_id,
                to: partition_id,
                kind: DependencyKind::Contains {
                    offset: Bytes::new(
                        partition["start"]
                            .as_u64()
                            .unwrap_or_else(|| unreachable!())
                            * 512,
                    ),
                },
            })
        );

        let fs_type = partition["fstype"]
            .as_str()
            .unwrap_or_else(|| unreachable!());
        let content = match fs_type {
            "swap" => NodeKind::Swap,
            "vfat" => NodeKind::Filesystem {
                kind: FilesystemKind::Vfat,
                label: None,
            },
            "ext4" => NodeKind::Filesystem {
                kind: FilesystemKind::Ext4,
                label: None,
            },
            other => NodeKind::Filesystem {
                kind: FilesystemKind::Other(other.into()),
                label: None,
            },
        };
        graph.insert_node(
            content_id,
            node(content, partition["size"].as_u64(), NodeFacts::default()),
        );
        assert!(graph.insert_dependency(Dependency {
            from: partition_id,
            to: content_id,
            kind: DependencyKind::Backs,
        }));
    }

    assert_eq!(graph.nodes().len(), 8);
    assert_eq!(graph.dependencies().len(), 7);
    roundtrip(&graph);
}

#[test]
/// Accepts successful empty reports from all three LVM reporting commands.
fn real_empty_lvm_json_reports_map_to_no_nodes() {
    let reports: Value = serde_json::from_str(include_str!("fixtures/lvm_empty.json"))
        .unwrap_or_else(|error| panic!("LVM fixture parses: {error}"));
    for (report, section) in reports
        .as_array()
        .unwrap_or_else(|| unreachable!())
        .iter()
        .zip(["pv", "vg", "lv"])
    {
        assert_eq!(
            report["report"][0][section]
                .as_array()
                .unwrap_or_else(|| unreachable!())
                .len(),
            0
        );
    }
}

#[test]
/// Builds original NVMe model types from three real `nvme-cli` JSON schemas.
fn real_nvme_cli_json_shapes_fit_existing_nvme_types() {
    let list: Value = serde_json::from_str(include_str!("fixtures/nvme_list.json"))
        .unwrap_or_else(|error| panic!("nvme list fixture parses: {error}"));
    let subsystems: Value = serde_json::from_str(include_str!("fixtures/nvme_list_subsys.json"))
        .unwrap_or_else(|error| panic!("nvme subsystems fixture parses: {error}"));
    let namespace: Value = serde_json::from_str(include_str!("fixtures/nvme_id_ns.json"))
        .unwrap_or_else(|error| panic!("nvme namespace fixture parses: {error}"));

    let device = &list["Devices"][0];
    let format = &namespace["lbafs"][namespace["flbas"]
        .as_u64()
        .unwrap_or_else(|| unreachable!()) as usize];
    let namespace = NvmeNamespace {
        nsid: NvmeNamespaceId::new(
            u32::try_from(
                device["NameSpace"]
                    .as_u64()
                    .unwrap_or_else(|| unreachable!()),
            )
            .unwrap_or_else(|error| panic!("namespace ID fits u32: {error}")),
        )
        .unwrap_or_else(|| unreachable!()),
        format: NvmeLbaFormat {
            data_size: BlockSize::new(
                1_u32
                    .checked_shl(
                        u32::try_from(format["ds"].as_u64().unwrap_or_else(|| unreachable!()))
                            .unwrap_or_else(|error| panic!("LBA exponent fits u32: {error}")),
                    )
                    .unwrap_or_else(|| unreachable!()),
            )
            .unwrap_or_else(|| unreachable!()),
            metadata_size: u16::try_from(format["ms"].as_u64().unwrap_or_else(|| unreachable!()))
                .unwrap_or_else(|error| panic!("metadata size fits u16: {error}")),
        },
    };
    let subsystem = NodeKind::NvmeSubsystem {
        nqn: subsystems[0]["Subsystems"][0]["NQN"]
            .as_str()
            .unwrap_or_else(|| unreachable!())
            .into(),
    };
    let transport = match subsystems[0]["Subsystems"][0]["Paths"][0]["Transport"].as_str() {
        Some("pcie") => Transport::Nvme,
        Some(_) => Transport::NvmeOf,
        None => unreachable!(),
    };

    assert_eq!(namespace.nsid.get(), 1);
    assert_eq!(namespace.format.data_size.get(), 512);
    assert_eq!(transport, Transport::Nvme);
    roundtrip(&(subsystem, NodeKind::NvmeNamespace(namespace)));
}
