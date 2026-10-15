//! Typed user requests for the initial plan editing workflow.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use storage_core::model::{
    Bytes, FilesystemKind, LvmLvKind, MountContext, MountEntry, MountSource, NodeId,
    OperationIntent,
};

/// User-level parameters retained for editing instead of mutating a frozen action DAG.
/// Representable requests may still be unsupported or semantically invalid when lowered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanRequest {
    /// Creates one logical volume, optionally followed by activation.
    CreateLv {
        /// Existing or planned volume group.
        vg: NodeId,
        /// Requested logical-volume name.
        name: String,
        /// Exact requested size in bytes.
        size: Bytes,
        /// Allocation layout; the first recipe will support linear volumes.
        layout: LvmLvKind,
        /// Whether the created volume should be activated.
        activate: bool,
    },
    /// Creates one filesystem on a referenced block node.
    CreateFilesystem {
        /// Existing or planned block-providing node.
        backing: NodeId,
        /// Filesystem implementation; the first recipe will support ext4.
        filesystem: FilesystemKind,
        /// Optional initial filesystem label.
        label: Option<String>,
    },
    /// Adds a runtime mount in the entry's context.
    Mount(MountEntry),
    /// Saves a persistent mount entry independently of runtime mounts.
    PersistMount(MountEntry),
    /// Removes a runtime mount.
    Unmount {
        /// Absolute mount target.
        target: PathBuf,
        /// Namespace containing the target.
        context: MountContext,
    },
    /// Removes a saved mount entry independently of runtime mounts.
    RemovePersistentMount {
        /// Absolute target in the saved configuration.
        target: PathBuf,
        /// Configuration context containing the entry.
        context: MountContext,
    },
    /// Changes the label of an existing or planned filesystem.
    SetFilesystemLabel {
        /// Filesystem whose label is changed.
        filesystem: NodeId,
        /// New label, or no label.
        label: Option<String>,
    },
    /// Retains an explicit maintenance intent; unsupported variants must fail lowering.
    Maintenance(OperationIntent),
}

impl PlanRequest {
    /// Whether this request reserves exactly one new storage node.
    pub(crate) fn creates_node(&self) -> bool {
        matches!(self, Self::CreateLv { .. } | Self::CreateFilesystem { .. })
    }

    /// Storage identities used as inputs; ordering and shared resources are not ownership.
    pub fn referenced_nodes(&self) -> Vec<NodeId> {
        match self {
            Self::CreateLv { vg, .. } => vec![*vg],
            Self::CreateFilesystem { backing, .. } => vec![*backing],
            Self::SetFilesystemLabel { filesystem, .. } => vec![*filesystem],
            Self::Mount(entry) | Self::PersistMount(entry) => match &entry.source {
                MountSource::Filesystem(node)
                | MountSource::BtrfsSubvolume(node)
                | MountSource::ZfsDataset(node) => vec![*node],
                MountSource::Bind(_)
                | MountSource::Tmpfs
                | MountSource::Network(_)
                | MountSource::Other(_) => Vec::new(),
            },
            Self::Unmount { .. } | Self::RemovePersistentMount { .. } => Vec::new(),
            Self::Maintenance(intent) => match intent {
                OperationIntent::Reformat { target, .. }
                | OperationIntent::Check { target, .. }
                | OperationIntent::Repair { target, .. }
                | OperationIntent::SetActivation { target, .. } => vec![*target],
                OperationIntent::ReplaceMember { group, old, new } => vec![*group, *old, *new],
                OperationIntent::ConvertBtrfsProfiles { filesystem, .. } => vec![*filesystem],
            },
        }
    }
}
