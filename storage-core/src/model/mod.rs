//! Canonical storage graph model.

use std::{collections::HashSet, num::NonZeroU32};

use serde::{Deserialize, Serialize};

// Dependency order: primitive value types, storage layouts, nodes, then graph edges.
mod block;
// Entrypoint module
mod graph;
mod node;
mod raid;
mod sizes;
mod state;

pub use block::*;
pub use graph::{
    BtrfsMember, CacheRole, Dependency, DependencyKind, MdMember, MdMemberRole, MemberRole,
    NodeGraph, Relation, RelationKind,
};
pub use node::{Node, NodeFacts, NodeId, NodeKind, NodeSpec, Presence, ProbeEpoch};
pub use raid::{
    BtrfsProfile, CopyCount, DataStripeCount, LvmRaid, LvmRaid0Variant, LvmRaid5Layout,
    LvmRaid6Layout, MdArray, MdExternalMetadata, MdMetadata, MdNativeMetadata, MdParityLayout,
    MdPersonality, MdRaid, MdRaid0Layout, MdRaid6DedicatedQ, MdRaid6Layout, MdRaid10Layout,
    MdRaid10Mode, ParityRotation,
};
pub use sizes::{Bytes, Mib};
pub use state::*;

/// Write policy of a dm-cache device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DmCacheMode {
    /// Cache writes before committing them to origin storage.
    Writeback,
    /// Commit writes to cache and origin storage together.
    Writethrough,
    /// Bypass cache promotion and write policy.
    Passthrough,
}

/// Write policy of a bcache device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BcacheMode {
    /// Cache writes before committing them to backing storage.
    Writeback,
    /// Commit writes to cache and backing storage together.
    Writethrough,
    /// Write directly to backing storage without caching the write.
    Writearound,
    /// Disable caching.
    None,
}

/// LUKS on-disk format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LuksVersion {
    /// LUKS1 format.
    Luks1,
    /// LUKS2 format.
    Luks2,
}

/// Btrfs allocation profiles used by each chunk class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtrfsAllocation {
    /// Profiles used for data chunks.
    pub data: HashSet<BtrfsProfile>,
    /// Profiles used for metadata chunks.
    pub metadata: HashSet<BtrfsProfile>,
    /// Profiles used for system chunks.
    pub system: HashSet<BtrfsProfile>,
}

/// Filesystem implementation and implementation-specific allocation data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilesystemKind {
    /// Ext2 filesystem.
    Ext2,
    /// Ext3 filesystem.
    Ext3,
    /// Ext4 filesystem.
    Ext4,
    /// XFS filesystem.
    Xfs,
    /// Btrfs filesystem and its allocation profiles.
    Btrfs(BtrfsAllocation),
    /// JFS filesystem.
    Jfs,
    /// ReiserFS filesystem.
    Reiserfs,
    /// FAT filesystem.
    Vfat,
    /// NTFS filesystem.
    Ntfs,
    /// exFAT filesystem.
    Exfat,
    /// F2FS filesystem.
    F2fs,
    /// OCFS2 filesystem.
    Ocfs2,
    /// GFS2 filesystem.
    Gfs2,
    /// APFS filesystem.
    Apfs,
    /// Filesystem not represented by a dedicated variant.
    Other(String),
}

impl FilesystemKind {
    /// Returns the provider-facing filesystem name.
    pub const fn as_str(&self) -> &str {
        match self {
            Self::Ext2 => "ext2",
            Self::Ext3 => "ext3",
            Self::Ext4 => "ext4",
            Self::Xfs => "xfs",
            Self::Btrfs(..) => "btrfs",
            Self::Jfs => "jfs",
            Self::Reiserfs => "reiserfs",
            Self::Vfat => "vfat",
            Self::Ntfs => "ntfs",
            Self::Exfat => "exfat",
            Self::F2fs => "f2fs",
            Self::Ocfs2 => "ocfs2",
            Self::Gfs2 => "gfs2",
            Self::Apfs => "apfs",
            Self::Other(other) => other.as_str(),
        }
    }
}

/// Compatibility name used by the legacy backend during migration.
pub use FilesystemKind as Fs;

/// LVM logical volume properties.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LvmLv {
    /// Logical volume name.
    pub name: String,
    /// Logical volume layout or role.
    pub kind: LvmLvKind,
}

/// LVM logical volume layout or role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LvmLvKind {
    /// Linear logical volume.
    Linear,
    /// Striped logical volume.
    Striped { stripes: DataStripeCount },
    /// LVM RAID logical volume.
    Raid(LvmRaid),
    /// Mirrored logical volume.
    Mirror { copies: CopyCount },
    /// Thin-pool logical volume.
    ThinPool,
    /// Thin logical volume.
    Thin,
    /// Snapshot logical volume.
    Snapshot,
    /// Cache-pool logical volume.
    CachePool,
    /// Cached logical volume.
    Cache,
    /// Write-cache logical volume.
    WriteCache,
    /// Provider-specific logical volume kind.
    Other(String),
}

/// NVMe namespace properties.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeNamespace {
    /// Namespace identifier.
    pub nsid: NvmeNamespaceId,
    /// Active logical block format.
    pub format: NvmeLbaFormat,
}

/// NVMe logical block format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeLbaFormat {
    /// Logical block data size.
    pub data_size: BlockSize,
    /// Metadata bytes stored with each logical block.
    pub metadata_size: u16,
}

/// Non-zero NVMe namespace identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[repr(transparent)]
pub struct NvmeNamespaceId(NonZeroU32);

impl NvmeNamespaceId {
    /// Creates a non-zero NVMe namespace identifier.
    pub const fn new(nsid: u32) -> Option<Self> {
        match NonZeroU32::new(nsid) {
            Some(nsid) => Some(Self(nsid)),
            None => None,
        }
    }

    /// Returns the namespace identifier.
    pub const fn get(&self) -> u32 {
        self.0.get()
    }
}

/// Native partition attribute bits and legacy boot flag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionAttributes {
    /// Raw GPT attribute mask, preserved without interpretation.
    pub gpt: Option<u64>,
    /// MBR bootable flag, when applicable.
    pub bootable: Option<bool>,
}

/// Intended system role of a partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageRole {
    /// Boot partition.
    Boot,
    /// Root filesystem partition.
    Root,
    /// Home filesystem partition.
    Home,
    /// Ordinary data partition.
    Data,
}

/// On-disk partition table format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PartitionTable {
    /// GUID Partition Table.
    Gpt,
    /// Master Boot Record partition table.
    Mbr,
}

/// Role of a device inside a ZFS virtual-device tree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
pub enum ZfsMember {
    /// Child of another virtual device.
    Child,
    /// Top-level virtual device in the given allocation class.
    TopLevel(ZfsVdevClass),
}

/// ZFS virtual-device layout.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZfsVdevKind {
    /// Single leaf device.
    Leaf,
    /// Mirrored devices.
    Mirror,
    /// RAIDZ layout.
    RaidZ { parity: ZfsParity },
    /// Distributed RAID layout.
    DRaid {
        /// Parity width.
        parity: ZfsParity,
        /// Data-device width.
        data_width: DataStripeCount,
        /// Number of distributed spare devices.
        distributed_spares: u16,
    },
    /// Provider-specific virtual-device layout.
    Other(String),
}

/// ZFS top-level virtual-device allocation class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ZfsVdevClass {
    /// Ordinary data devices.
    Data,
    /// Intent-log devices.
    Log,
    /// Special allocation devices.
    Special,
    /// Deduplication table devices.
    Dedup,
    /// Read-cache devices.
    Cache,
    /// Spare devices.
    Spare,
}

/// Number of parity devices in RAIDZ or dRAID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ZfsParity {
    /// One parity device.
    One,
    /// Two parity devices.
    Two,
    /// Three parity devices.
    Three,
}

#[cfg(test)]
mod provider_compatibility;
