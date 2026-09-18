//! Block-device observations, geometry, and stable identities.

use std::{num::NonZeroU32, path::PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Bytes;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockFacts {
    pub paths: Vec<PathBuf>,
    pub devno: DeviceNumber,
    pub geometry: Option<BlockGeometry>,
    pub read_only: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceNumber {
    pub major: u32,
    pub minor: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockGeometry {
    pub logical_block_size: BlockSize,
    pub physical_block_size: Option<BlockSize>,
    pub alignment_offset: Option<Bytes>,
    pub minimum_io_size: Option<Bytes>,
    pub optimal_io_size: Option<Bytes>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub transport: Option<Transport>,
    /// Network backing detected behind an already mapped virtual block device.
    pub network_backing: Option<NetworkBacking>,
    pub rotational: Option<bool>,
    pub removable: Option<bool>,
    /// Zoned block-device model reported by sysfs or a transport provider.
    pub zoned: Option<ZonedModel>,
}

impl DeviceInfo {
    /// Reports whether the observed device is reached through a network storage transport.
    pub const fn is_network(&self) -> Option<bool> {
        match (self.network_backing, self.transport) {
            (Some(_), _) => Some(true),
            (None, Some(transport)) => Some(transport.is_network()),
            (None, None) => None,
        }
    }
}

/// Network protocol backing an already mapped virtual block device.
///
/// This is observation-only metadata and does not imply lifecycle support for
/// connecting, mapping, or disconnecting the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkBacking {
    /// Ceph RADOS Block Device mapping.
    Rbd,
    /// Network Block Device mapping.
    Nbd,
}

/// Zoned block-device behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZonedModel {
    HostAware,
    HostManaged,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Nvme,
    NvmeOf,
    Sata,
    Sas,
    Scsi,
    Usb,
    Virtio,
    Mmc,
    FibreChannel,
    Iscsi,
}

impl Transport {
    /// Reports whether this transport represents remote or network-backed storage.
    pub const fn is_network(self) -> bool {
        matches!(self, Self::NvmeOf | Self::FibreChannel | Self::Iscsi)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExternalId {
    Wwn(String),
    Serial {
        vendor: Option<String>,
        value: String,
    },
    GptPartitionUuid(Uuid),
    MbrPartitionId {
        disk_signature: u32,
        number: NonZeroU32,
    },
    LuksUuid(Uuid),
    MdUuid(String),
    LvmPvUuid(String),
    LvmVgUuid(String),
    LvmLvUuid(String),
    Filesystem {
        fs_type: String,
        value: String,
    },
    BtrfsFsid(Uuid),
    BtrfsSubvolumeUuid(Uuid),
    ZfsGuid(u64),
    NvmeNguid([u8; 16]),
    NvmeEui64([u8; 8]),
    NvmeUuid(Uuid),
    Iscsi {
        target_iqn: String,
        lun: u64,
    },
    NvmeOf {
        subsystem_nqn: String,
        namespace: NonZeroU32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockSize(NonZeroU32);

impl BlockSize {
    /// Creates a non-zero block size in bytes.
    pub const fn new(bytes: u32) -> Option<Self> {
        match NonZeroU32::new(bytes) {
            Some(bytes) => Some(Self(bytes)),
            None => None,
        }
    }

    /// Returns the block size in bytes.
    pub const fn get(&self) -> u32 {
        self.0.get()
    }
}
