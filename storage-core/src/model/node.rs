//! Storage node identity, state, and intrinsic kinds.

use std::{num::NonZeroU32, path::PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    BcacheMode, BlockFacts, Bytes, DeviceInfo, DmCacheMode, ExternalId, FilesystemKind,
    LuksVersion, LvmLv, MdArray, NvmeNamespace, PartitionAttributes, PartitionTable, UsageRole,
    ZfsVdevKind,
};

/// Identity of a storage node.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(Uuid);

impl NodeId {
    /// Creates a fresh transient node identity.
    pub fn new() -> Self {
        NodeId(uuid::Uuid::now_v7())
    }

    /// Creates a node identity from a provider-resolved UUID.
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Returns the underlying UUID for serialization boundaries.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonical storage object without runtime observations or relationships.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Intrinsic semantic kind and properties.
    pub kind: NodeSpec,
    /// Capacity provided to the next storage layer.
    pub size: NodeFacts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub kind: NodeKind,
    pub size: Option<Bytes>,
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProbeEpoch(u64);

impl ProbeEpoch {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeFacts {
    pub observed_in: Option<ProbeEpoch>,
    pub presence: Presence,
    pub identities: Vec<ExternalId>,
    pub block: Option<BlockFacts>,
    pub device: Option<DeviceInfo>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Presence {
    #[default]
    Unknown,
    Present,
    Missing,
}

/// Intrinsic semantic kind and properties of a storage node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    /// Physical disk.
    Disk,
    NvmeSubsystem {
        nqn: String,
    },
    NvmeController,
    /// NVMe namespace.
    NvmeNamespace(NvmeNamespace),
    /// File-backed loop device.
    Loop {
        backing_file: Option<PathBuf>,
    },
    /// Compressed in-memory block device.
    Zram,
    /// DmMultipath block device.
    DmMultipath,
    /// Partition table.
    PartitionTable(PartitionTable),
    /// Partition described relative to its table by a `Contains` edge.
    Partition {
        number: NonZeroU32,
        offset: Bytes,
        role: Option<UsageRole>,
        attributes: PartitionAttributes,
    },
    /// On-disk LUKS container.
    LuksContainer {
        version: LuksVersion,
    },
    /// Open dm-crypt mapping.
    DmCryptMapping {
        name: String,
    },
    /// dm-integrity mapping.
    DmIntegrity {
        name: String,
    },
    /// dm-verity mapping.
    DmVerity {
        name: String,
    },
    /// Linux MD array.
    MdArray(MdArray),
    /// LVM physical volume.
    LvmPv,
    /// LVM volume group.
    LvmVg {
        name: String,
        extent_size: Option<Bytes>,
    },
    /// LVM logical volume.
    LvmLv(LvmLv),
    /// bcache device.
    Bcache {
        name: String,
        mode: BcacheMode,
    },
    /// dm-cache device.
    DmCache {
        name: String,
        mode: DmCacheMode,
    },
    /// dm-writecache device.
    DmWritecache {
        name: String,
    },
    /// Filesystem, including multi-device Btrfs.
    Filesystem {
        kind: FilesystemKind,
        label: Option<String>,
    },
    /// Swap area.
    Swap,
    /// Btrfs subvolume.
    BtrfsSubvolume {
        name: String,
        read_only: bool,
        is_default: bool,
    },
    /// ZFS storage pool.
    ZfsPool {
        name: String,
    },
    /// ZFS virtual device.
    ZfsVdev(ZfsVdevKind),
    /// ZFS dataset.
    ZfsDataset {
        name: String,
    },
    /// ZFS block volume.
    ZfsVolume {
        name: String,
    },
    /// DRBD resource.
    Drbd {
        name: String,
    },
    /// Source object which cannot yet be classified.
    Unknown(String),
}
