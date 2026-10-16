//! Typed user requests for the initial plan editing workflow.

use std::{num::NonZeroU32, path::PathBuf};

use serde::{Deserialize, Serialize};
use storage_core::model::{
    Bytes, FilesystemKind, LuksVersion, LvmLvKind, MountContext, MountEntry, MountSource, NodeId,
    OperationIntent, PartitionAttributes, UsageRole,
};

use storage_provider::CredentialRef;

/// User-level parameters retained for editing instead of mutating a frozen action DAG.
/// Representable requests may still be unsupported or semantically invalid when lowered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanRequest {
    /// Creates one partition with exact byte geometry on an existing or planned table.
    CreatePartition {
        /// Partition table receiving the new entry.
        table: NodeId,
        /// Explicit nonzero partition number; no automatic placement is performed.
        number: NonZeroU32,
        /// Exact byte offset from the beginning of the disk.
        offset: Bytes,
        /// Exact requested size in bytes.
        size: Bytes,
        /// Optional intended system role.
        role: Option<UsageRole>,
        /// Native attributes preserved without implicit defaults or reinterpretation.
        attributes: PartitionAttributes,
    },
    /// Prepares a LUKS header on a block node; execution requires destructive-scope approval.
    FormatLuks {
        /// Existing or planned block node whose content would be replaced.
        backing: NodeId,
        /// Requested on-disk header version.
        version: LuksVersion,
        /// Opaque external credential-store identity, never credential material.
        credential: CredentialRef,
    },
    /// Opens a LUKS container as a separately identified dm-crypt mapping.
    OpenLuks {
        /// Existing or planned LUKS container.
        container: NodeId,
        /// Explicit device-mapper name.
        mapping_name: String,
        /// Opaque external credential-store identity, resolved only by the runtime.
        credential: CredentialRef,
        /// Whether the resulting mapping rejects writes.
        read_only: bool,
    },
    /// Creates one logical volume, optionally followed by activation.
    CreateLv {
        /// Existing or planned volume group.
        vg: NodeId,
        /// Requested logical-volume name.
        name: String,
        /// Exact requested size in bytes.
        size: Bytes,
        /// Allocation layout; the initial recipe supports linear volumes.
        layout: LvmLvKind,
        /// Whether the created volume should be activated.
        activate: bool,
    },
    /// Creates one filesystem on a referenced block node.
    CreateFilesystem {
        /// Existing or planned block-providing node.
        backing: NodeId,
        /// Filesystem implementation; the initial recipe supports ext4.
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
        matches!(
            self,
            Self::CreatePartition { .. }
                | Self::FormatLuks { .. }
                | Self::OpenLuks { .. }
                | Self::CreateLv { .. }
                | Self::CreateFilesystem { .. }
        )
    }

    /// Storage identities used as inputs; ordering and shared resources are not ownership.
    pub fn referenced_nodes(&self) -> Vec<NodeId> {
        match self {
            Self::CreatePartition { table, .. } => vec![*table],
            Self::CreateLv { vg, .. } => vec![*vg],
            Self::CreateFilesystem { backing, .. } | Self::FormatLuks { backing, .. } => {
                vec![*backing]
            }
            Self::OpenLuks { container, .. } => vec![*container],
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

impl PlanRequest {
    /// Rebinds only typed inputs after correlation; output IDs and path text are never rewritten.
    pub(super) fn remap_nodes(&mut self, mapping: &std::collections::HashMap<NodeId, NodeId>) {
        let remap = |node: &mut NodeId| {
            if let Some(target) = mapping.get(node) {
                *node = *target;
            }
        };
        match self {
            Self::CreatePartition { table, .. } => remap(table),
            Self::CreateLv { vg, .. } => remap(vg),
            Self::CreateFilesystem { backing, .. } | Self::FormatLuks { backing, .. } => {
                remap(backing)
            }
            Self::OpenLuks { container, .. } => remap(container),
            Self::SetFilesystemLabel { filesystem, .. } => remap(filesystem),
            Self::Mount(entry) | Self::PersistMount(entry) => match &mut entry.source {
                MountSource::Filesystem(node)
                | MountSource::BtrfsSubvolume(node)
                | MountSource::ZfsDataset(node) => remap(node),
                MountSource::Bind(_)
                | MountSource::Tmpfs
                | MountSource::Network(_)
                | MountSource::Other(_) => {}
            },
            Self::Unmount { .. } | Self::RemovePersistentMount { .. } => {}
            Self::Maintenance(intent) => match intent {
                OperationIntent::Reformat { target, .. }
                | OperationIntent::Check { target, .. }
                | OperationIntent::Repair { target, .. }
                | OperationIntent::SetActivation { target, .. } => remap(target),
                OperationIntent::ReplaceMember { group, old, new } => {
                    remap(group);
                    remap(old);
                    remap(new);
                }
                OperationIntent::ConvertBtrfsProfiles { filesystem, .. } => remap(filesystem),
            },
        }
    }
}

#[cfg(test)]
mod remap_tests {
    use super::*;

    /// Rebinding changes typed inputs only, preserving options, namespace, and path strings.
    #[test]
    fn remapping_preserves_path_and_mount_semantics() {
        let source = NodeId::new();
        let target = NodeId::new();
        let mapping = std::collections::HashMap::from([(source, target)]);
        let entry = MountEntry {
            source: MountSource::Filesystem(source),
            target: "/data".into(),
            options: vec!["ro".to_owned(), "rw".to_owned()],
            context: MountContext::Host,
        };
        for mut request in [
            PlanRequest::FormatLuks {
                backing: source,
                version: LuksVersion::Luks2,
                credential: CredentialRef::from_uuid(source.as_uuid()),
            },
            PlanRequest::OpenLuks {
                container: source,
                mapping_name: "crypt-data".to_owned(),
                credential: CredentialRef::from_uuid(source.as_uuid()),
                read_only: true,
            },
            PlanRequest::Mount(entry.clone()),
            PlanRequest::PersistMount(entry.clone()),
            PlanRequest::SetFilesystemLabel {
                filesystem: source,
                label: Some("data".to_owned()),
            },
            PlanRequest::Maintenance(OperationIntent::ReplaceMember {
                group: source,
                old: source,
                new: source,
            }),
            PlanRequest::Maintenance(OperationIntent::Check {
                target: source,
                force: true,
            }),
        ] {
            request.remap_nodes(&mapping);
            assert!(
                request
                    .referenced_nodes()
                    .iter()
                    .all(|node| *node == target)
            );
            if let PlanRequest::FormatLuks { credential, .. }
            | PlanRequest::OpenLuks { credential, .. } = &request
            {
                assert_eq!(credential.as_uuid(), source.as_uuid());
            }
            if let PlanRequest::Mount(actual) | PlanRequest::PersistMount(actual) = request {
                assert_eq!(actual.target, entry.target);
                assert_eq!(actual.options, entry.options);
                assert_eq!(actual.context, entry.context);
            }
        }
        let mut bind = PlanRequest::Mount(MountEntry {
            source: MountSource::Bind("/dev/old".into()),
            ..entry
        });
        let original = bind.clone();
        bind.remap_nodes(&mapping);
        assert_eq!(bind, original);
    }
}
