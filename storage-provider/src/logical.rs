//! Contracts owned by logical storage-technology providers.

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use storage_core::model::{
    DependencyKind, Diagnostic, DiagnosticSeverity, DiagnosticSubject, MountContext, MountSource,
    NodeGraph, NodeId, NodeKind,
};

use crate::{BackendId, FilesystemAction, LvmAction, MountAction, PartitionAction, ProviderAction};

/// Runs backend-independent technology checks against the effective graph.
///
/// LUKS shares the same checks with `LogicalProvider::validate_action`.
/// Partition mutations require an unambiguous table metadata domain.
/// LVM validates ownership, nonzero size, and exact linear extent alignment.
/// Mount paths require stable absolute identities without parent traversal.
/// Other technologies currently rely on their action-contract prerequisites;
/// their parameter validators belong here as they are implemented.
pub fn validate_provider_action(action: &ProviderAction, graph: &NodeGraph) -> Vec<Diagnostic> {
    match action {
        ProviderAction::Luks(action) => crate::luks::validate_action(action, graph),
        ProviderAction::Lvm(action) => validate_lvm_action(action, graph),
        ProviderAction::Mount(action) => validate_mount_action(action, graph),
        ProviderAction::Filesystem(FilesystemAction::Resize {
            filesystem,
            new_size,
        }) => {
            let (code, message) = if new_size.as_u64() == 0 {
                (
                    "filesystem.resize.zero_size",
                    "filesystem capacity must be greater than zero",
                )
            } else if !crate::contract::is_ext_filesystem(graph, *filesystem) {
                (
                    "filesystem.resize.unsupported",
                    "filesystem resize currently supports only the ext2/ext3/ext4 recipe",
                )
            } else if graph
                .dependencies()
                .filter(|edge| edge.to == *filesystem && matches!(edge.kind, DependencyKind::Backs))
                .count()
                > 1
            {
                (
                    "filesystem.resize.ambiguous_backing",
                    "ext resize requires at most one direct backing node",
                )
            } else {
                return Vec::new();
            };
            vec![Diagnostic {
                code: code.to_owned(),
                severity: DiagnosticSeverity::Error,
                subjects: vec![DiagnosticSubject::Node(*filesystem)],
                message: message.to_owned(),
                evidence: None,
                suggested_remedy: None,
            }]
        }
        ProviderAction::Partition(
            PartitionAction::Resize { partition, .. }
            | PartitionAction::Delete { partition }
            | PartitionAction::SetAttributes { partition, .. },
        ) => {
            if partition_table(graph, *partition).is_some() {
                Vec::new()
            } else {
                vec![Diagnostic {
                    code: "partition.action.invalid_table_ownership".to_owned(),
                    severity: DiagnosticSeverity::Error,
                    subjects: vec![DiagnosticSubject::Node(*partition)],
                    message: "partition mutation requires exactly one containing partition table"
                        .to_owned(),
                    evidence: None,
                    suggested_remedy: None,
                }]
            }
        }
        ProviderAction::Partition(action @ PartitionAction::Create { .. }) => {
            validate_partition_create(action, graph)
        }
        ProviderAction::Partition(PartitionAction::CreateTable { .. })
        | ProviderAction::Filesystem(_)
        | ProviderAction::Btrfs(_) => Vec::new(),
    }
}

/// Rejects provably invalid extents; backend allocation/alignment evidence remains required.
fn validate_partition_create(action: &PartitionAction, graph: &NodeGraph) -> Vec<Diagnostic> {
    let PartitionAction::Create {
        table,
        number,
        offset,
        size,
        ..
    } = action
    else {
        return Vec::new();
    };
    let reject = |code: &str, message: &str| {
        vec![Diagnostic {
            code: code.to_owned(),
            severity: DiagnosticSeverity::Error,
            subjects: vec![DiagnosticSubject::Node(*table)],
            message: message.to_owned(),
            evidence: None,
            suggested_remedy: None,
        }]
    };
    let Some(end) = offset.as_u64().checked_add(size.as_u64()) else {
        return reject(
            "partition.create.invalid_extent",
            "partition byte range overflows",
        );
    };
    if size.as_u64() == 0
        || graph
            .node(table)
            .and_then(|node| node.kind.size)
            .is_some_and(|capacity| end > capacity.as_u64())
    {
        return reject(
            "partition.create.invalid_extent",
            "partition must have nonzero size and fit within known table capacity",
        );
    }
    let mut overlaps = false;
    for edge in graph
        .dependencies()
        .filter(|edge| edge.from == *table && matches!(edge.kind, DependencyKind::Contains { .. }))
    {
        let Some(node) = graph.node(&edge.to) else {
            continue;
        };
        let NodeKind::Partition {
            number: occupied_number,
            offset: occupied_offset,
            ..
        } = node.kind.kind
        else {
            continue;
        };
        if number == &occupied_number {
            return reject(
                "partition.create.duplicate_number",
                "partition number is already used in this table",
            );
        }
        if let Some(occupied_size) = node.kind.size {
            let occupied_end = occupied_offset.as_u64().checked_add(occupied_size.as_u64());
            overlaps |= occupied_end.is_none_or(|occupied_end| {
                offset.as_u64() < occupied_end && occupied_offset.as_u64() < end
            });
        }
    }
    if overlaps {
        return reject(
            "partition.create.overlap",
            "partition overlaps a known occupied or overflowing byte range",
        );
    }
    Vec::new()
}

/// Rejects ambiguous lexical paths without resolving filesystem state or symbolic links.
fn validate_mount_action(action: &MountAction, graph: &NodeGraph) -> Vec<Diagnostic> {
    let (target, context, source) = match action {
        MountAction::Mount {
            target,
            context,
            source,
            ..
        } => (target, context, Some(source)),
        MountAction::Persist { entry } => (&entry.target, &entry.context, Some(&entry.source)),
        MountAction::Unmount { target, context }
        | MountAction::RemovePersistent { target, context } => (target, context, None),
    };
    let mut paths = vec![(target.as_path(), "mount.target")];
    if let MountContext::TargetRoot(root) = context {
        paths.push((root.as_path(), "mount.context.target_root"));
    }
    if let Some(MountSource::Bind(source)) = source {
        paths.push((source.as_path(), "mount.source.bind"));
    }
    let mut diagnostics: Vec<_> = paths
        .into_iter()
        .filter(|(path, _)| !valid_mount_path(path))
        .map(|(path, field)| Diagnostic {
            code: "mount.action.invalid_path".to_owned(),
            severity: DiagnosticSeverity::Error,
            subjects: vec![
                DiagnosticSubject::Mount(path.to_path_buf()),
                DiagnosticSubject::Field(field.to_owned()),
            ],
            message: "mount paths must be absolute and contain neither '..' components nor NUL"
                .to_owned(),
            evidence: None,
            suggested_remedy: None,
        })
        .collect();
    // Persistent entries need a compatible object, but do not require runtime activation.
    if let MountAction::Persist { entry } = action
        && let Some(node) = crate::contract::mount_source_node(&entry.source)
        && let Some(value) = graph.node(&node)
        && !matches!(
            (&entry.source, &value.kind.kind),
            (MountSource::Filesystem(_), NodeKind::Filesystem { .. })
                | (
                    MountSource::BtrfsSubvolume(_),
                    NodeKind::BtrfsSubvolume { .. }
                )
                | (MountSource::ZfsDataset(_), NodeKind::ZfsDataset { .. })
        )
    {
        diagnostics.push(Diagnostic {
            code: "mount.action.incompatible_source".to_owned(),
            severity: DiagnosticSeverity::Error,
            subjects: vec![DiagnosticSubject::Node(node)],
            message: "persistent mount source has an incompatible node kind".to_owned(),
            evidence: None,
            suggested_remedy: None,
        });
    }
    diagnostics
}

/// Keeps mount resource identity independent of cwd, parent traversal, and invalid OS paths.
fn valid_mount_path(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| component == Component::ParentDir)
        && !path.as_os_str().as_encoded_bytes().contains(&0)
}

/// Rejects ambiguous allocation domains and sizes that cannot be represented exactly by extents.
fn validate_lvm_action(action: &LvmAction, graph: &NodeGraph) -> Vec<Diagnostic> {
    let (subject, vg, size, linear) =
        match action {
            LvmAction::CreateLv {
                vg, size, layout, ..
            } => (
                *vg,
                *vg,
                *size,
                matches!(layout, storage_core::model::LvmLvKind::Linear),
            ),
            LvmAction::ResizeLv { lv, new_size } => {
                let Some(vg) = crate::contract::lvm_volume_group(graph, *lv) else {
                    return vec![lvm_action_diagnostic(
                        *lv,
                        "lvm.action.invalid_vg_ownership",
                        "LV mutation requires exactly one owning volume group",
                    )];
                };
                let linear = graph.node(lv).is_some_and(|node| matches!(&node.kind.kind,
                NodeKind::LvmLv(value) if value.kind == storage_core::model::LvmLvKind::Linear
            ));
                (*lv, vg, *new_size, linear)
            }
            LvmAction::RemoveLv { lv } => {
                return if crate::contract::lvm_volume_group(graph, *lv).is_some() {
                    Vec::new()
                } else {
                    vec![lvm_action_diagnostic(
                        *lv,
                        "lvm.action.invalid_vg_ownership",
                        "LV mutation requires exactly one owning volume group",
                    )]
                };
            }
            LvmAction::CreatePv { .. }
            | LvmAction::CreateVg { .. }
            | LvmAction::SetActivation { .. } => return Vec::new(),
        };
    if size.as_u64() == 0 {
        return vec![lvm_action_diagnostic(
            subject,
            "lvm.action.zero_size",
            "LV capacity must be greater than zero",
        )];
    }
    if let Some(node) = graph.node(&vg)
        && let NodeKind::LvmVg {
            extent_size: Some(extent),
            ..
        } = node.kind.kind
    {
        if extent.as_u64() == 0 {
            return vec![lvm_action_diagnostic(
                subject,
                "lvm.action.invalid_extent_size",
                "VG extent size must be greater than zero",
            )];
        }
        if linear && !size.as_u64().is_multiple_of(extent.as_u64()) {
            return vec![lvm_action_diagnostic(
                subject,
                "lvm.action.unaligned_size",
                "requested linear LV capacity must be an exact multiple of the VG extent size",
            )];
        }
    }
    Vec::new()
}

/// Builds a stable node-scoped LVM parameter or ownership diagnostic.
fn lvm_action_diagnostic(node: NodeId, code: &str, message: &str) -> Diagnostic {
    Diagnostic {
        code: code.to_owned(),
        severity: DiagnosticSeverity::Error,
        subjects: vec![DiagnosticSubject::Node(node)],
        message: message.to_owned(),
        evidence: None,
        suggested_remedy: None,
    }
}

/// Resolves exactly one containing partition table without choosing a hash iteration order.
pub(crate) fn partition_table(graph: &NodeGraph, partition: NodeId) -> Option<NodeId> {
    let mut owners = graph.dependencies().filter(|dependency| {
        dependency.to == partition && matches!(dependency.kind, DependencyKind::Contains { .. })
    });
    if let Some(owner) = owners.next()
        && owners.next().is_none()
        && graph
            .node(&owner.from)
            .is_some_and(|node| matches!(node.kind.kind, NodeKind::PartitionTable(_)))
    {
        return Some(owner.from);
    }
    None
}

/// Stable identity of a logical provider such as `luks` or `lvm`.
#[repr(transparent)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    /// Creates a provider ID without whitespace.
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty() && !value.chars().any(char::is_whitespace)).then_some(Self(value))
    }

    /// Returns the opaque provider ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ProviderId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| {
            <D::Error as serde::de::Error>::custom(
                "provider ID must be non-empty and contain no whitespace",
            )
        })
    }
}

/// Technology semantics presented to validation and planning.
///
/// The associated action stays provider-owned. A future plugin transport must
/// carry its serialized representation without turning it into a C symbol or
/// a command line.
pub trait LogicalProvider {
    /// Typed action understood by this provider.
    type Action;

    /// Returns the stable logical-provider identity.
    fn id(&self) -> &ProviderId;

    /// Returns the concrete system backend selected for this provider.
    fn backend_id(&self) -> &BackendId;

    /// Checks technology-specific constraints without changing the system.
    fn validate_action(&self, action: &Self::Action, graph: &NodeGraph) -> Vec<Diagnostic>;
}

#[cfg(test)]
mod tests {
    use storage_core::model::{
        Bytes, Dependency, Node, NodeFacts, NodeSpec, PartitionAttributes, PartitionTable,
    };

    use super::*;

    /// Relative paths, parent traversal, and NUL cannot bypass mount resource identity.
    #[test]
    fn mount_paths_are_validated_for_runtime_and_persistent_actions() {
        for path in [
            "",
            "relative",
            "/data/../child",
            "/data\0child",
            "/data//./child",
        ] {
            let valid = path == "/data//./child";
            let target = std::path::PathBuf::from(path);
            let entry = storage_core::model::MountEntry {
                source: MountSource::Tmpfs,
                target: target.clone(),
                options: Vec::new(),
                context: MountContext::Host,
            };
            for action in [
                MountAction::Mount {
                    source: MountSource::Tmpfs,
                    target: target.clone(),
                    options: Vec::new(),
                    context: MountContext::Host,
                },
                MountAction::Unmount {
                    target: target.clone(),
                    context: MountContext::Host,
                },
                MountAction::Persist { entry },
                MountAction::RemovePersistent {
                    target: target.clone(),
                    context: MountContext::Host,
                },
            ] {
                let diagnostics =
                    validate_provider_action(&ProviderAction::Mount(action), &NodeGraph::new());
                assert_eq!(diagnostics.is_empty(), valid);
            }
        }
        for action in [
            MountAction::Mount {
                source: MountSource::Tmpfs,
                target: "/data".into(),
                options: Vec::new(),
                context: MountContext::TargetRoot("/target/../root".into()),
            },
            MountAction::Mount {
                source: MountSource::Bind("relative".into()),
                target: "/data".into(),
                options: Vec::new(),
                context: MountContext::Host,
            },
        ] {
            let diagnostics =
                validate_provider_action(&ProviderAction::Mount(action), &NodeGraph::new());
            assert!(
                matches!(diagnostics.as_slice(), [diagnostic] if diagnostic.code == "mount.action.invalid_path")
            );
        }
    }

    /// LVM sizes are never silently rounded, and mutations cannot choose an arbitrary VG.
    #[test]
    fn lvm_sizes_and_allocation_domains_are_validated() {
        let vg = NodeId::new();
        let lv = NodeId::new();
        let mut graph = NodeGraph::new();
        graph
            .insert_new(
                vg,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::LvmVg {
                            name: "vg0".to_owned(),
                            extent_size: Some(Bytes::new(4)),
                        },
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        graph
            .insert_new(
                lv,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::LvmLv(storage_core::model::LvmLv {
                            name: "data".to_owned(),
                            kind: storage_core::model::LvmLvKind::Linear,
                        }),
                        size: Some(Bytes::new(60)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let ownership = Dependency {
            from: vg,
            to: lv,
            kind: DependencyKind::Provides,
        };
        graph.insert_dependency(ownership.clone());
        for (size, code) in [
            (0, Some("lvm.action.zero_size")),
            (62, Some("lvm.action.unaligned_size")),
            (64, None),
        ] {
            for action in [
                LvmAction::CreateLv {
                    vg,
                    planned_node_id: NodeId::new(),
                    name: "new".to_owned(),
                    size: Bytes::new(size),
                    layout: storage_core::model::LvmLvKind::Linear,
                },
                LvmAction::ResizeLv {
                    lv,
                    new_size: Bytes::new(size),
                },
            ] {
                let diagnostics = validate_provider_action(&ProviderAction::Lvm(action), &graph);
                if let Some(code) = code {
                    assert!(
                        matches!(diagnostics.as_slice(), [diagnostic] if diagnostic.code == code)
                    );
                } else {
                    assert!(diagnostics.is_empty());
                }
            }
        }
        let mut invalid_extent = graph.clone();
        invalid_extent
            .replace_existing(
                vg,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::LvmVg {
                            name: "vg0".to_owned(),
                            extent_size: Some(Bytes::new(0)),
                        },
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let diagnostics = validate_provider_action(
            &ProviderAction::Lvm(LvmAction::ResizeLv {
                lv,
                new_size: Bytes::new(64),
            }),
            &invalid_extent,
        );
        assert!(
            matches!(diagnostics.as_slice(), [diagnostic] if diagnostic.code == "lvm.action.invalid_extent_size")
        );
        let other_vg = NodeId::new();
        graph
            .insert_new(
                other_vg,
                graph
                    .node(&vg)
                    .cloned()
                    .unwrap_or_else(|| panic!("VG is absent")),
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        for owner_count in [0, 1, 2] {
            graph.remove_dependency(&ownership);
            let competing = Dependency {
                from: other_vg,
                to: lv,
                kind: DependencyKind::Provides,
            };
            graph.remove_dependency(&competing);
            if owner_count >= 1 {
                graph.insert_dependency(ownership.clone());
            }
            if owner_count == 2 {
                graph.insert_dependency(competing);
            }
            for action in [
                LvmAction::ResizeLv {
                    lv,
                    new_size: Bytes::new(64),
                },
                LvmAction::RemoveLv { lv },
            ] {
                let diagnostics = validate_provider_action(&ProviderAction::Lvm(action), &graph);
                if owner_count == 1 {
                    assert!(diagnostics.is_empty());
                } else {
                    assert!(
                        matches!(diagnostics.as_slice(), [diagnostic] if diagnostic.code == "lvm.action.invalid_vg_ownership")
                    );
                }
            }
        }
    }

    /// Unknown, invalid, or competing ownership cannot bypass partition metadata locking.
    #[test]
    fn partition_mutations_require_one_valid_metadata_domain() {
        let partition = NodeId::new();
        let first = NodeId::new();
        let second = NodeId::new();
        for owner_count in [0, 1, 2] {
            for valid_kind in [false, true] {
                let mut graph = NodeGraph::new();
                for table in [first, second].into_iter().take(owner_count) {
                    graph
                        .insert_new(
                            table,
                            Node {
                                kind: NodeSpec {
                                    kind: if valid_kind {
                                        NodeKind::PartitionTable(PartitionTable::Gpt)
                                    } else {
                                        NodeKind::Disk
                                    },
                                    size: None,
                                },
                                size: NodeFacts::default(),
                            },
                        )
                        .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
                    graph.insert_dependency(Dependency {
                        from: table,
                        to: partition,
                        kind: DependencyKind::Contains {
                            offset: Bytes::new(0),
                        },
                    });
                }
                for operation in [
                    PartitionAction::Delete { partition },
                    PartitionAction::Resize {
                        partition,
                        new_size: Bytes::new(60),
                    },
                    PartitionAction::SetAttributes {
                        partition,
                        attributes: PartitionAttributes::default(),
                    },
                ] {
                    let diagnostics =
                        validate_provider_action(&ProviderAction::Partition(operation), &graph);
                    if owner_count == 1 && valid_kind {
                        assert!(diagnostics.is_empty());
                        assert_eq!(partition_table(&graph, partition), Some(first));
                    } else {
                        assert!(matches!(diagnostics.as_slice(), [diagnostic]
                            if diagnostic.code == "partition.action.invalid_table_ownership"
                                && diagnostic.severity == DiagnosticSeverity::Error));
                    }
                }
            }
        }
    }

    /// The ext resize recipe cannot choose an arbitrary backing node from ambiguous metadata.
    #[test]
    fn filesystem_resize_rejects_multiple_backing_nodes() -> Result<(), Box<dyn std::error::Error>>
    {
        let filesystem = NodeId::new();
        let mut graph = NodeGraph::new();
        graph.insert_new(
            filesystem,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::Filesystem {
                        kind: storage_core::model::FilesystemKind::Ext4,
                        label: None,
                    },
                    size: Some(Bytes::new(100)),
                },
                size: NodeFacts::default(),
            },
        )?;
        let action = ProviderAction::Filesystem(FilesystemAction::Resize {
            filesystem,
            new_size: Bytes::new(60),
        });
        for count in 0..=2 {
            let diagnostics = validate_provider_action(&action, &graph);
            assert_eq!(diagnostics.is_empty(), count < 2);
            if count == 2 {
                assert_eq!(diagnostics[0].code, "filesystem.resize.ambiguous_backing");
                break;
            }
            let backing = NodeId::new();
            graph.insert_new(
                backing,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: Some(Bytes::new(100)),
                    },
                    size: NodeFacts::default(),
                },
            )?;
            graph.insert_dependency(Dependency {
                from: backing,
                to: filesystem,
                kind: DependencyKind::Backs,
            });
        }
        Ok(())
    }

    /// Invalid ranges and occupied numbers are rejected, while adjacent ranges remain possible.
    #[test]
    fn partition_creation_checks_known_ranges_and_numbers() {
        let table = NodeId::new();
        let occupied = NodeId::new();
        let mut graph = NodeGraph::new();
        for (id, kind, size) in [
            (
                table,
                NodeKind::PartitionTable(PartitionTable::Gpt),
                Some(Bytes::new(1000)),
            ),
            (
                occupied,
                NodeKind::Partition {
                    number: std::num::NonZeroU32::new(1)
                        .unwrap_or_else(|| panic!("nonzero fixture number")),
                    offset: Bytes::new(100),
                    role: None,
                    attributes: PartitionAttributes::default(),
                },
                Some(Bytes::new(100)),
            ),
        ] {
            graph
                .insert_new(
                    id,
                    Node {
                        kind: NodeSpec { kind, size },
                        size: NodeFacts::default(),
                    },
                )
                .unwrap_or_else(|error| panic!("fixture: {error:?}"));
        }
        graph.insert_dependency(Dependency {
            from: table,
            to: occupied,
            kind: DependencyKind::Contains {
                offset: Bytes::new(100),
            },
        });
        for (number, offset, size, expected) in [
            (2, 0, 0, Some("partition.create.invalid_extent")),
            (2, u64::MAX, 2, Some("partition.create.invalid_extent")),
            (2, 900, 101, Some("partition.create.invalid_extent")),
            (1, 200, 100, Some("partition.create.duplicate_number")),
            (2, 99, 2, Some("partition.create.overlap")),
            (2, 150, 10, Some("partition.create.overlap")),
            (2, 199, 2, Some("partition.create.overlap")),
            (2, 0, 100, None),
            (2, 200, 800, None),
        ] {
            let action = ProviderAction::Partition(PartitionAction::Create {
                table,
                planned_node_id: NodeId::new(),
                number: std::num::NonZeroU32::new(number)
                    .unwrap_or_else(|| panic!("nonzero fixture number")),
                offset: Bytes::new(offset),
                size: Bytes::new(size),
                role: None,
                attributes: PartitionAttributes::default(),
            });
            let diagnostics = validate_provider_action(&action, &graph);
            assert_eq!(
                diagnostics.first().map(|d| d.code.as_str()),
                expected,
                "{action:?}"
            );
        }
    }

    /// Verifies that JSON cannot bypass provider-ID constructor invariants.
    #[test]
    fn provider_id_rejects_invalid_json() {
        assert!(serde_json::from_str::<ProviderId>(r#""""#).is_err());
        assert!(serde_json::from_str::<ProviderId>(r#""two words""#).is_err());
    }
}
