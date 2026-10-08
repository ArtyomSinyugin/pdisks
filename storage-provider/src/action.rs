//! Closed, serializable provider actions consumed by planning and execution.
//!
//! Actions contain storage intent only. They never contain backend handles,
//! function pointers, closures, library paths, or arbitrary command arguments.

use std::{num::NonZeroU32, path::PathBuf};

use serde::{Deserialize, Serialize};
use storage_core::model::{
    BtrfsAllocation, Bytes, FilesystemKind, LvmLvKind, MountContext, MountEntry, MountSource,
    NodeId, PartitionAttributes, PartitionTable, UsageRole,
};

use crate::LuksAction;

/// Typed operation dispatched by its logical technology provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", content = "operation", rename_all = "snake_case")]
pub enum ProviderAction {
    /// Partition-table operation owned by the partition provider.
    Partition(PartitionAction),
    /// Encryption operation owned by the LUKS provider.
    Luks(LuksAction),
    /// Volume-management operation owned by the LVM provider.
    Lvm(LvmAction),
    /// Ordinary filesystem operation owned by the filesystem provider.
    Filesystem(FilesystemAction),
    /// Btrfs-specific operation owned by the Btrfs provider.
    Btrfs(BtrfsAction),
    /// Runtime or persistent mount operation owned by the mount provider.
    Mount(MountAction),
}

/// Typed partition-table operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionAction {
    /// Creates a partition-table node on an existing disk.
    CreateTable {
        /// Existing disk or block device receiving the table.
        disk: NodeId,
        /// Stable identity reserved for the planned table node.
        planned_node_id: NodeId,
        /// Partition-table format to create.
        table: PartitionTable,
    },
    /// Creates one partition within an existing table.
    Create {
        /// Existing partition-table node.
        table: NodeId,
        /// Stable identity reserved for the planned partition node.
        planned_node_id: NodeId,
        /// Non-zero partition number requested from the backend.
        number: NonZeroU32,
        /// Byte offset from the start of the containing device.
        offset: Bytes,
        /// Exact partition size in bytes.
        size: Bytes,
        /// Optional semantic role requested for the partition.
        role: Option<UsageRole>,
        /// Native and legacy partition attributes.
        attributes: PartitionAttributes,
    },
    /// Resizes an existing partition without changing its identity.
    Resize {
        /// Existing partition node.
        partition: NodeId,
        /// Requested exact partition size.
        new_size: Bytes,
    },
    /// Deletes an existing partition.
    Delete {
        /// Existing partition node.
        partition: NodeId,
    },
    /// Changes native or legacy attributes of an existing partition.
    SetAttributes {
        /// Existing partition node.
        partition: NodeId,
        /// Complete requested attribute state.
        attributes: PartitionAttributes,
    },
}

/// Typed LVM operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LvmAction {
    /// Initializes an existing block node as an LVM physical volume.
    CreatePv {
        /// Existing block-providing node.
        backing: NodeId,
        /// Stable identity reserved for the planned PV node.
        planned_node_id: NodeId,
    },
    /// Creates a volume group from existing PV nodes.
    CreateVg {
        /// Stable identity reserved for the planned VG node.
        planned_node_id: NodeId,
        /// Requested volume-group name.
        name: String,
        /// Physical-volume members of the new group.
        pvs: Vec<NodeId>,
    },
    /// Creates a logical volume in an existing volume group.
    CreateLv {
        /// Existing volume-group node.
        vg: NodeId,
        /// Stable identity reserved for the planned LV node.
        planned_node_id: NodeId,
        /// Requested logical-volume name.
        name: String,
        /// Exact capacity exposed by the logical volume.
        size: Bytes,
        /// Logical-volume layout or role.
        layout: LvmLvKind,
    },
    /// Resizes an existing logical volume.
    ResizeLv {
        /// Existing logical-volume node.
        lv: NodeId,
        /// Requested exact logical-volume size.
        new_size: Bytes,
    },
    /// Removes an existing logical volume.
    RemoveLv {
        /// Existing logical-volume node.
        lv: NodeId,
    },
    /// Changes runtime activation of a volume group or logical volume.
    SetActivation {
        /// Existing LVM node.
        target: NodeId,
        /// Requested activation state.
        active: bool,
    },
}

/// Typed operations for ordinary filesystems owned by `libbd_fs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemAction {
    /// Creates a filesystem on an existing backing node.
    Create {
        /// Existing block-providing node.
        backing: NodeId,
        /// Stable identity reserved for the planned filesystem node.
        planned_node_id: NodeId,
        /// Filesystem implementation to create.
        filesystem: FilesystemKind,
        /// Optional requested filesystem label.
        label: Option<String>,
    },
    /// Resizes an existing filesystem.
    Resize {
        /// Existing filesystem node.
        filesystem: NodeId,
        /// Requested exact capacity.
        new_size: Bytes,
    },
    /// Performs a read-only or provider-default filesystem check.
    Check {
        /// Existing filesystem node.
        filesystem: NodeId,
        /// Whether the provider may bypass its normal clean-state shortcut.
        force: bool,
    },
    /// Repairs an existing filesystem.
    Repair {
        /// Existing filesystem node.
        filesystem: NodeId,
        /// Whether the provider may use forceful repair behavior.
        force: bool,
    },
    /// Changes the label of an existing filesystem.
    SetLabel {
        /// Existing filesystem node.
        filesystem: NodeId,
        /// Complete requested label, or `None` to clear it.
        label: Option<String>,
    },
}

/// Typed Btrfs-specific operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BtrfsAction {
    /// Creates one Btrfs filesystem across one or more block members.
    Create {
        /// Existing block-providing member nodes.
        members: Vec<NodeId>,
        /// Stable identity reserved for the planned filesystem node.
        planned_node_id: NodeId,
        /// Optional requested filesystem label.
        label: Option<String>,
        /// Requested data, metadata, and system allocation profiles.
        allocation: BtrfsAllocation,
    },
    /// Adds a block member to an existing Btrfs filesystem.
    AddDevice {
        /// Existing Btrfs filesystem node.
        filesystem: NodeId,
        /// Existing block-providing node to add.
        device: NodeId,
    },
    /// Removes a block member from an existing Btrfs filesystem.
    RemoveDevice {
        /// Existing Btrfs filesystem node.
        filesystem: NodeId,
        /// Existing member node to remove.
        device: NodeId,
    },
    /// Converts allocation profiles without changing filesystem identity.
    ConvertProfiles {
        /// Existing Btrfs filesystem node.
        filesystem: NodeId,
        /// Complete requested allocation profile state.
        target: BtrfsAllocation,
    },
    /// Creates a Btrfs subvolume.
    CreateSubvolume {
        /// Existing Btrfs filesystem node.
        filesystem: NodeId,
        /// Stable identity reserved for the planned subvolume node.
        planned_node_id: NodeId,
        /// Requested subvolume name relative to its filesystem root.
        name: String,
        /// Whether the new subvolume must be read-only.
        read_only: bool,
    },
}

/// Typed runtime and persistent mount operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountAction {
    /// Mounts one source in a selected namespace context.
    Mount {
        /// Storage, bind, temporary, or network mount source.
        source: MountSource,
        /// Mount target interpreted within `context`.
        target: PathBuf,
        /// Normalized mount options.
        options: Vec<String>,
        /// Filesystem namespace in which the mount is created.
        context: MountContext,
    },
    /// Unmounts one target from a selected namespace context.
    Unmount {
        /// Existing mount target interpreted within `context`.
        target: PathBuf,
        /// Filesystem namespace containing the mount.
        context: MountContext,
    },
    /// Writes or replaces one managed persistent mount entry.
    Persist {
        /// Complete desired persistent mount entry.
        entry: MountEntry,
    },
    /// Removes one managed persistent mount entry.
    RemovePersistent {
        /// Persistent mount target to remove.
        target: PathBuf,
        /// Target-root or host context owning the configuration file.
        context: MountContext,
    },
}

#[cfg(test)]
mod tests {
    use storage_core::model::{DataStripeCount, LvmLvKind};

    use super::*;

    /// Ensures the closed provider tag and typed operation survive JSON round-trip.
    #[test]
    fn lvm_create_lv_round_trips_as_provider_data() {
        let action = ProviderAction::Lvm(LvmAction::CreateLv {
            vg: NodeId::new(),
            planned_node_id: NodeId::new(),
            name: "root".to_owned(),
            size: Bytes::new(16 * 1024 * 1024 * 1024),
            layout: LvmLvKind::Striped {
                stripes: DataStripeCount::new(2).unwrap_or_else(|| unreachable!()),
            },
        });
        let json = serde_json::to_string(&action)
            .unwrap_or_else(|error| panic!("serialize provider action: {error}"));
        let restored: ProviderAction = serde_json::from_str(&json)
            .unwrap_or_else(|error| panic!("deserialize provider action: {error}"));

        assert_eq!(restored, action);
        assert!(json.contains(r#""provider":"lvm""#));
        assert!(!json.contains("libbd_lvm"));
        assert!(!json.contains("command"));
    }

    /// Ensures filesystem creation names its planned output explicitly.
    #[test]
    fn filesystem_create_preserves_planned_node() {
        let planned_node_id = NodeId::new();
        let action = ProviderAction::Filesystem(FilesystemAction::Create {
            backing: NodeId::new(),
            planned_node_id,
            filesystem: FilesystemKind::Ext4,
            label: Some("root".to_owned()),
        });
        let json = serde_json::to_value(&action)
            .unwrap_or_else(|error| panic!("serialize filesystem action: {error}"));

        assert_eq!(json["provider"], "filesystem");
        assert_eq!(
            json["operation"]["create"]["planned_node_id"],
            planned_node_id.as_uuid().to_string()
        );
    }
}
