//! Three-valued checks against desired configuration and temporary evidence.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use crate::simulation_evidence::{
    CheckEvidence, CheckStatus, PrerequisiteCheck, SimulationEvidence, is_observation_stale,
};
use storage_core::model::{
    DependencyKind, DesiredState, FilesystemKind, MountContext, MountEntry, MountSource, NodeId,
    NodeKind,
};
use storage_provider::{
    ActionPrerequisite, AllocationDomain, AllocationQuantity, BlockInterfaceAccess, FactKind,
    FactScope, NodeKindClass,
};

/// Evaluates action prerequisites against the current read-only projection.
pub(crate) fn evaluate_prerequisites(
    desired: &DesiredState,
    evidence: &SimulationEvidence,
    prerequisites: &[ActionPrerequisite],
) -> Vec<PrerequisiteCheck> {
    prerequisites
        .iter()
        .cloned()
        .map(|prerequisite| PrerequisiteCheck {
            status: evaluate_prerequisite(desired, evidence, &prerequisite),
            prerequisite,
        })
        .collect()
}

/// Evaluates one prerequisite without changing projected state.
fn evaluate_prerequisite(
    desired: &DesiredState,
    evidence: &SimulationEvidence,
    prerequisite: &ActionPrerequisite,
) -> CheckStatus {
    match prerequisite {
        ActionPrerequisite::NodeExists(node) => match desired.graph.node(node) {
            Some(_) => satisfied_for_node(evidence, *node),
            None => CheckStatus::Violated {
                reason: "referenced node is absent".to_owned(),
            },
        },
        ActionPrerequisite::NodeKind { node, expected } => {
            let Some(node_value) = desired.graph.node(node) else {
                return CheckStatus::Violated {
                    reason: "referenced node is absent".to_owned(),
                };
            };
            if matches_kind(&node_value.kind.kind, *expected) {
                satisfied_for_node(evidence, *node)
            } else {
                CheckStatus::Violated {
                    reason: "node kind does not satisfy the action contract".to_owned(),
                }
            }
        }
        ActionPrerequisite::NoUnexpectedConsumers(node) => {
            if desired.graph.node(node).is_none() {
                CheckStatus::Violated {
                    reason: "referenced node is absent".to_owned(),
                }
            } else if desired
                .graph
                .dependencies()
                .any(|dependency| dependency.from == *node)
            {
                CheckStatus::Violated {
                    reason: "node still has an upper-layer consumer".to_owned(),
                }
            } else {
                satisfied_for_node(evidence, *node)
            }
        }
        ActionPrerequisite::Writable(node) => evaluate_endpoint(desired, evidence, *node, true),
        ActionPrerequisite::MountSourceAvailable(source) => {
            evaluate_mount_source(desired, evidence, source)
        }
        ActionPrerequisite::FilesystemUnmounted(filesystem) => {
            if desired.graph.node(filesystem).is_none() {
                CheckStatus::Violated {
                    reason: "referenced filesystem is absent".to_owned(),
                }
            } else if desired
                .mounts
                .runtime
                .iter()
                .any(|mount| mount.source == MountSource::Filesystem(*filesystem))
            {
                CheckStatus::Violated {
                    reason: "filesystem is still mounted in an observed namespace".to_owned(),
                }
            } else if desired
                .mounts
                .runtime
                .iter()
                .any(|mount| matches!(mount.source, MountSource::Bind(_) | MountSource::Other(_)))
            {
                CheckStatus::Unknown {
                    required_evidence:
                        "resolve bind and unsupported mount sources before offline filesystem work"
                            .to_owned(),
                }
            } else {
                CheckStatus::Satisfied(mount_evidence(evidence))
            }
        }
        ActionPrerequisite::FilesystemCheckPassed(filesystem) => {
            if evidence.checked_filesystems.contains(filesystem) {
                CheckStatus::Satisfied(CheckEvidence::PredictedEffect)
            } else {
                CheckStatus::Violated {
                    reason: "filesystem shrink requires a preceding fresh forced read-only check"
                        .to_owned(),
                }
            }
        }
        ActionPrerequisite::FilesystemMinimumSize { .. } => CheckStatus::Unknown {
            required_evidence:
                "provider evidence that filesystem content fits the requested shrink target"
                    .to_owned(),
        },
        ActionPrerequisite::DestructiveApproval(_) => CheckStatus::Unknown {
            required_evidence: "explicit destructive-scope approval".to_owned(),
        },
        ActionPrerequisite::ExtentAvailable { .. } => CheckStatus::Unknown {
            required_evidence: "provider allocation and alignment evidence".to_owned(),
        },
        ActionPrerequisite::DistinctNodes(nodes) => {
            let distinct = nodes.iter().copied().collect::<HashSet<_>>();
            if distinct.len() == nodes.len() {
                CheckStatus::Satisfied(CheckEvidence::PredictedEffect)
            } else {
                CheckStatus::Violated {
                    reason: "node list contains duplicate identities".to_owned(),
                }
            }
        }
        ActionPrerequisite::DependentsFitWithin { node, size } => {
            evaluate_dependent_sizes(desired, evidence, *node, *size)
        }
        ActionPrerequisite::BackingSizeAtLeast { node, size } => {
            evaluate_backing_size(desired, evidence, *node, *size)
        }
        ActionPrerequisite::CapacityAvailable { domain, amount } => {
            evaluate_capacity(evidence, domain, *amount)
        }
        ActionPrerequisite::MountTargetAvailable { target, context } => {
            if mount_at(desired, target, context).is_some() {
                CheckStatus::Violated {
                    reason: "mount target is already occupied".to_owned(),
                }
            } else {
                CheckStatus::Satisfied(mount_evidence(evidence))
            }
        }
        ActionPrerequisite::MountDescendantsAbsent { target, context } => {
            if has_mounted_descendant(desired, target, context) {
                CheckStatus::Violated {
                    reason: "mount target still contains an active descendant".to_owned(),
                }
            } else {
                CheckStatus::Satisfied(mount_evidence(evidence))
            }
        }
    }
}

/// Resolves endpoint availability and optional write access without trusting missing facts.
fn evaluate_endpoint(
    desired: &DesiredState,
    evidence: &SimulationEvidence,
    node: NodeId,
    writable: bool,
) -> CheckStatus {
    let Some(node_value) = desired.graph.node(&node) else {
        return CheckStatus::Violated {
            reason: "referenced node is absent".to_owned(),
        };
    };
    // An explicit deactivation supersedes endpoint observations from the baseline.
    if evidence.activations.get(&node) == Some(&false) {
        return CheckStatus::Violated {
            reason: "projected block endpoint is inactive".to_owned(),
        };
    }
    match evidence.block_interfaces.get(&node) {
        Some(BlockInterfaceAccess::ReadWrite) => {
            return CheckStatus::Satisfied(CheckEvidence::PredictedEffect);
        }
        Some(BlockInterfaceAccess::ReadOnly) if !writable => {
            return CheckStatus::Satisfied(CheckEvidence::PredictedEffect);
        }
        Some(BlockInterfaceAccess::ReadOnly) => {
            return CheckStatus::Violated {
                reason: "projected block interface is read-only".to_owned(),
            };
        }
        None => {}
    }
    if is_observation_stale(evidence, &FactScope::Node(node), FactKind::Activation)
        || is_observation_stale(evidence, &FactScope::Node(node), FactKind::Presence)
    {
        return CheckStatus::Unknown {
            required_evidence: "fresh block endpoint observation after runtime invalidation"
                .to_owned(),
        };
    }
    if !writable {
        return if node_value.size.block.is_some() {
            CheckStatus::Satisfied(CheckEvidence::ObservedSnapshot)
        } else {
            CheckStatus::Unknown {
                required_evidence: "available block endpoint observation".to_owned(),
            }
        };
    }
    match node_value
        .size
        .block
        .as_ref()
        .and_then(|block| block.read_only)
    {
        Some(false) => CheckStatus::Satisfied(CheckEvidence::ObservedSnapshot),
        Some(true) => CheckStatus::Violated {
            reason: "observed block endpoint is read-only".to_owned(),
        },
        None => CheckStatus::Unknown {
            required_evidence: "writable block endpoint observation".to_owned(),
        },
    }
}

/// Checks simple filesystem backing; multi-device and dataset health need live evidence.
fn evaluate_mount_source(
    desired: &DesiredState,
    evidence: &SimulationEvidence,
    source: &MountSource,
) -> CheckStatus {
    let node = match source {
        MountSource::Filesystem(node)
        | MountSource::BtrfsSubvolume(node)
        | MountSource::ZfsDataset(node) => *node,
        _ => {
            return CheckStatus::Unknown {
                required_evidence: "provider mount-source availability check".to_owned(),
            };
        }
    };
    let Some(value) = desired.graph.node(&node) else {
        return CheckStatus::Violated {
            reason: "mount source is absent".to_owned(),
        };
    };
    let compatible = matches!(
        (source, &value.kind.kind),
        (MountSource::Filesystem(_), NodeKind::Filesystem { .. })
            | (
                MountSource::BtrfsSubvolume(_),
                NodeKind::BtrfsSubvolume { .. }
            )
            | (MountSource::ZfsDataset(_), NodeKind::ZfsDataset { .. })
    );
    if !compatible {
        return CheckStatus::Violated {
            reason: "mount source has an incompatible node kind".to_owned(),
        };
    }
    if matches!(&value.kind.kind, NodeKind::Filesystem { kind, .. }
        if !matches!(kind, FilesystemKind::Btrfs(_)))
    {
        let backing: Vec<_> = desired
            .graph
            .dependencies()
            .filter(|dependency| {
                dependency.to == node && matches!(dependency.kind, DependencyKind::Backs)
            })
            .map(|dependency| dependency.from)
            .collect();
        if let [backing] = backing.as_slice()
            && desired
                .graph
                .node(backing)
                .is_some_and(|value| matches_kind(&value.kind.kind, NodeKindClass::Block))
        {
            return evaluate_endpoint(desired, evidence, *backing, false);
        }
    }
    CheckStatus::Unknown {
        required_evidence: "provider mount-source topology and availability check".to_owned(),
    }
}

/// Checks direct block consumers against a proposed lower-layer capacity.
fn evaluate_dependent_sizes(
    desired: &DesiredState,
    evidence: &SimulationEvidence,
    node: NodeId,
    size: storage_core::model::Bytes,
) -> CheckStatus {
    if desired.graph.node(&node).is_none() {
        return CheckStatus::Violated {
            reason: "referenced node is absent".to_owned(),
        };
    }
    let mut found = false;
    let mut predicted = evidence.projected_graph_nodes.contains(&node);
    for dependency in desired.graph.dependencies().filter(|dependency| {
        dependency.from == node && matches!(dependency.kind, DependencyKind::Backs)
    }) {
        found = true;
        predicted |= evidence.projected_graph_nodes.contains(&dependency.to);
        let Some(dependent) = desired.graph.node(&dependency.to) else {
            return CheckStatus::Violated {
                reason: "dependent node is absent".to_owned(),
            };
        };
        let Some(dependent_size) = dependent.kind.size else {
            return CheckStatus::Unknown {
                required_evidence: "dependent capacity observation".to_owned(),
            };
        };
        if dependent_size > size {
            return CheckStatus::Violated {
                reason: "an upper layer exceeds the proposed backing capacity".to_owned(),
            };
        }
    }
    if found {
        CheckStatus::Satisfied(if predicted {
            CheckEvidence::PredictedEffect
        } else {
            CheckEvidence::ObservedSnapshot
        })
    } else {
        satisfied_for_node(evidence, node)
    }
}

/// Checks the direct block backing against a proposed upper-layer capacity.
fn evaluate_backing_size(
    desired: &DesiredState,
    evidence: &SimulationEvidence,
    node: NodeId,
    size: storage_core::model::Bytes,
) -> CheckStatus {
    if desired.graph.node(&node).is_none() {
        return CheckStatus::Violated {
            reason: "referenced node is absent".to_owned(),
        };
    }
    let Some(dependency) = desired.graph.dependencies().find(|dependency| {
        dependency.to == node && matches!(dependency.kind, DependencyKind::Backs)
    }) else {
        return CheckStatus::Unknown {
            required_evidence: "direct backing relationship".to_owned(),
        };
    };
    let Some(backing) = desired.graph.node(&dependency.from) else {
        return CheckStatus::Violated {
            reason: "backing node is absent".to_owned(),
        };
    };
    let Some(backing_size) = backing.kind.size else {
        return CheckStatus::Unknown {
            required_evidence: "backing capacity observation".to_owned(),
        };
    };
    if backing_size < size {
        CheckStatus::Violated {
            reason: "backing capacity is smaller than the requested size".to_owned(),
        }
    } else {
        CheckStatus::Satisfied(
            if evidence.projected_graph_nodes.contains(&node)
                || evidence.projected_graph_nodes.contains(&dependency.from)
            {
                CheckEvidence::PredictedEffect
            } else {
                CheckEvidence::ObservedSnapshot
            },
        )
    }
}

/// Checks cumulative free capacity in one provider allocation domain.
fn evaluate_capacity(
    evidence: &SimulationEvidence,
    domain: &AllocationDomain,
    amount: Option<AllocationQuantity>,
) -> CheckStatus {
    let Some(amount) = amount else {
        return CheckStatus::Unknown {
            required_evidence: "provider allocation calculation for this layout".to_owned(),
        };
    };
    let Some(available) = evidence.available_capacity.get(domain) else {
        return CheckStatus::Unknown {
            required_evidence: "provider allocation-capacity evidence".to_owned(),
        };
    };
    if *available < amount {
        CheckStatus::Violated {
            reason: "allocation domain has insufficient free capacity".to_owned(),
        }
    } else if evidence.projected_allocations.contains(domain) {
        CheckStatus::Satisfied(CheckEvidence::PredictedEffect)
    } else {
        CheckStatus::Satisfied(CheckEvidence::ObservedSnapshot)
    }
}

/// Identifies whether a satisfied fact came from the snapshot or projection.
fn satisfied_for_node(evidence: &SimulationEvidence, node: NodeId) -> CheckStatus {
    let evidence = if evidence.projected_graph_nodes.contains(&node) {
        CheckEvidence::PredictedEffect
    } else {
        CheckEvidence::ObservedSnapshot
    };
    CheckStatus::Satisfied(evidence)
}

/// Finds an effective runtime mount by namespace and target.
pub(crate) fn mount_at<'a>(
    desired: &'a DesiredState,
    target: &PathBuf,
    context: &MountContext,
) -> Option<&'a MountEntry> {
    desired
        .mounts
        .runtime
        .iter()
        .find(|mount| mount.target == *target && mount.context == *context)
}

/// Tests component-wise ancestry within one namespace, excluding the target itself.
pub(crate) fn has_mounted_descendant(
    desired: &DesiredState,
    target: &Path,
    context: &MountContext,
) -> bool {
    desired.mounts.runtime.iter().any(|mount| {
        mount.context == *context && mount.target != target && mount.target.starts_with(target)
    })
}

/// Identifies whether effective mount evidence includes a projected transition.
fn mount_evidence(evidence: &SimulationEvidence) -> CheckEvidence {
    if evidence.projected_runtime_mounts {
        CheckEvidence::PredictedEffect
    } else {
        CheckEvidence::ObservedSnapshot
    }
}

/// Reports whether a node kind satisfies a coarse provider contract class.
pub(crate) fn matches_kind(kind: &NodeKind, expected: NodeKindClass) -> bool {
    match expected {
        NodeKindClass::Block => matches!(
            kind,
            NodeKind::Disk
                | NodeKind::NvmeNamespace(_)
                | NodeKind::Loop { .. }
                | NodeKind::Zram
                | NodeKind::DmMultipath
                | NodeKind::Partition { .. }
                | NodeKind::LuksContainer { .. }
                | NodeKind::DmCryptMapping { .. }
                | NodeKind::DmIntegrity { .. }
                | NodeKind::DmVerity { .. }
                | NodeKind::MdArray(_)
                | NodeKind::LvmPv
                | NodeKind::LvmLv(_)
                | NodeKind::Bcache { .. }
                | NodeKind::DmCache { .. }
                | NodeKind::DmWritecache { .. }
                | NodeKind::ZfsVolume { .. }
                | NodeKind::Drbd { .. }
        ),
        NodeKindClass::PartitionTable => matches!(kind, NodeKind::PartitionTable(_)),
        NodeKindClass::Partition => matches!(kind, NodeKind::Partition { .. }),
        NodeKindClass::LuksContainer => matches!(kind, NodeKind::LuksContainer { .. }),
        NodeKindClass::DmCryptMapping => matches!(kind, NodeKind::DmCryptMapping { .. }),
        NodeKindClass::LvmPv => matches!(kind, NodeKind::LvmPv),
        NodeKindClass::LvmVg => matches!(kind, NodeKind::LvmVg { .. }),
        NodeKindClass::LvmLv => matches!(kind, NodeKind::LvmLv(_)),
        NodeKindClass::Filesystem => matches!(kind, NodeKind::Filesystem { .. }),
        NodeKindClass::BtrfsFilesystem => matches!(
            kind,
            NodeKind::Filesystem {
                kind: FilesystemKind::Btrfs(_),
                ..
            }
        ),
    }
}
