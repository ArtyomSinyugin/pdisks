//! Trusted semantic contracts derived from typed provider actions.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use storage_core::model::{
    BtrfsAllocation, BtrfsMember, Bytes, Dependency, DependencyKind, FilesystemKind, MemberRole,
    MountContext, MountEntry, MountSource, NodeGraph, NodeId, NodeKind, NodeSpec, Relation,
};
use thiserror::Error;

use crate::{
    BtrfsAction, FilesystemAction, LuksAction, LvmAction, MountAction, PartitionAction,
    ProviderAction,
};

/// Read-only model context used to derive a trusted action contract.
#[derive(Debug, Clone, Copy)]
pub struct ActionDescriptionContext<'graph> {
    /// Current or simulated graph visible before the action.
    pub graph: &'graph NodeGraph,
}

/// Complete semantic contract derived from one typed action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionContract {
    /// Conditions that a trusted executor must verify before dispatch.
    pub prerequisites: Vec<ActionPrerequisite>,
    /// Model-visible objects made available after successful verification.
    pub provided_outputs: Vec<ActionOutput>,
    /// Resources read or changed by the action.
    pub affected_resources: Vec<ResourceAccess>,
    /// Single predicted model/configuration effect shared with simulation.
    pub predicted_effect: PredictedEffect,
    /// Observations that cannot be reused after dispatch.
    pub invalidated_facts: Vec<FactInvalidation>,
    /// Evidence required to accept backend completion as success.
    pub postconditions: Vec<Postcondition>,
    /// Permitted recovery behavior after interruption or an unknown result.
    pub restart_policy: RestartPolicy,
}

/// Condition that must be freshly enforced before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionPrerequisite {
    /// A referenced node must exist in the effective graph.
    NodeExists(NodeId),
    /// A referenced node must have a compatible semantic kind.
    NodeKind {
        /// Node checked before dispatch.
        node: NodeId,
        /// Required semantic kind class.
        expected: NodeKindClass,
    },
    /// No unplanned upper-layer consumer may depend on this node.
    NoUnexpectedConsumers(NodeId),
    /// The backing endpoint must not be observed read-only.
    Writable(NodeId),
    /// Destructive use of the named node requires explicit approval.
    DestructiveApproval(NodeId),
    /// A partition-table extent must be free and correctly aligned.
    ExtentAvailable {
        /// Partition-table node owning the address space.
        table: NodeId,
        /// Requested byte offset.
        offset: Bytes,
        /// Requested byte length.
        size: Bytes,
    },
    /// Every listed node must be distinct.
    DistinctNodes(Vec<NodeId>),
    /// A mount target must not already be occupied in the selected context.
    MountTargetAvailable {
        /// Requested target path.
        target: PathBuf,
        /// Namespace context containing the path.
        context: MountContext,
    },
}

/// Coarse node-kind requirement used by provider contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKindClass {
    /// Any node capable of backing another block/content layer.
    Block,
    /// Partition-table node.
    PartitionTable,
    /// Partition node.
    Partition,
    /// LUKS container node.
    LuksContainer,
    /// Open dm-crypt mapping node.
    DmCryptMapping,
    /// LVM physical-volume node.
    LvmPv,
    /// LVM volume-group node.
    LvmVg,
    /// LVM logical-volume node.
    LvmLv,
    /// Ordinary or Btrfs filesystem node.
    Filesystem,
    /// Btrfs filesystem node specifically.
    BtrfsFilesystem,
}

/// Output made available by successful action verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutput {
    /// Planned graph node with host-known semantic properties.
    Node {
        /// Stable planned identity.
        id: NodeId,
        /// Predictable node specification; backend-generated facts are absent.
        spec: NodeSpec,
    },
    /// Runtime mount made available in one namespace.
    Mount {
        /// Mounted source.
        source: MountSource,
        /// Mount target.
        target: PathBuf,
        /// Namespace context containing the mount.
        context: MountContext,
    },
    /// Persistent mount configuration made available to the target system.
    PersistentMount(MountEntry),
}

/// One resource touched by an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceAccess {
    /// Resource identity used for plan locking and conflict checks.
    pub resource: ActionResource,
    /// Whether the action observes or changes the resource.
    pub mode: ResourceAccessMode,
}

/// Resource identity relevant to plan ordering and locking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionResource {
    /// Canonical storage node.
    Node(NodeId),
    /// Partition-table metadata domain.
    PartitionTable(NodeId),
    /// LVM volume-group metadata domain.
    LvmMetadata(NodeId),
    /// Runtime mount target in a namespace.
    MountTarget {
        /// Target path.
        target: PathBuf,
        /// Namespace context containing the target.
        context: MountContext,
    },
    /// Persistent mount configuration belonging to a context.
    PersistentMountConfig(MountContext),
}

/// Access mode requested for one action resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccessMode {
    /// Resource is observed but not intentionally changed.
    Read,
    /// Resource or its metadata is intentionally changed.
    Write,
}

/// Model and configuration changes predicted from an action.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictedEffect {
    /// Changes to the canonical storage graph.
    pub graph: Vec<GraphEffect>,
    /// Changes to runtime or persistent mounts.
    pub mounts: Vec<MountEffect>,
}

/// One predicted canonical-graph change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphEffect {
    /// Inserts a planned node without inventing runtime facts.
    InsertNode {
        /// Stable planned identity.
        id: NodeId,
        /// Predictable semantic specification.
        spec: NodeSpec,
    },
    /// Removes a node after postcondition verification.
    RemoveNode(NodeId),
    /// Changes capacity exposed by an existing node.
    SetNodeSize {
        /// Existing node whose size changes.
        node: NodeId,
        /// Requested new capacity.
        size: Bytes,
    },
    /// Changes partition attributes without replacing the partition.
    SetPartitionAttributes {
        /// Existing partition node.
        partition: NodeId,
        /// Complete requested attribute state.
        attributes: storage_core::model::PartitionAttributes,
    },
    /// Changes a filesystem label without replacing its identity.
    SetFilesystemLabel {
        /// Existing filesystem node.
        filesystem: NodeId,
        /// Complete requested label state.
        label: Option<String>,
    },
    /// Changes Btrfs allocation profiles.
    SetBtrfsAllocation {
        /// Existing Btrfs filesystem node.
        filesystem: NodeId,
        /// Complete requested allocation profile state.
        allocation: BtrfsAllocation,
    },
    /// Inserts a dependency edge.
    InsertDependency(Dependency),
    /// Removes a dependency edge.
    RemoveDependency(Dependency),
    /// Inserts a non-owning relation edge.
    InsertRelation(Relation),
    /// Removes a non-owning relation edge.
    RemoveRelation(Relation),
    /// Predicts a runtime activation request not represented by `NodeSpec`.
    SetActivation {
        /// Existing node whose runtime state changes.
        node: NodeId,
        /// Requested activation state.
        active: bool,
    },
}

/// One predicted runtime or persistent mount change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountEffect {
    /// Adds a runtime mount.
    AddRuntime {
        /// Mounted source.
        source: MountSource,
        /// Mount target.
        target: PathBuf,
        /// Normalized mount options.
        options: Vec<String>,
        /// Namespace context containing the mount.
        context: MountContext,
    },
    /// Removes a runtime mount.
    RemoveRuntime {
        /// Mount target.
        target: PathBuf,
        /// Namespace context containing the mount.
        context: MountContext,
    },
    /// Writes or replaces a managed persistent entry.
    SetPersistent(MountEntry),
    /// Removes a managed persistent entry.
    RemovePersistent {
        /// Configured mount target.
        target: PathBuf,
        /// Context owning the persistent configuration.
        context: MountContext,
    },
}

/// Observation invalidated by dispatching an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactInvalidation {
    /// Model or configuration scope requiring a fresh probe.
    pub scope: FactScope,
    /// Category of stale evidence.
    pub kind: FactKind,
}

/// Scope whose observed facts become stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactScope {
    /// Canonical node and the named aspect of its observations.
    Node(NodeId),
    /// Runtime mount target in a namespace.
    MountTarget {
        /// Mount target.
        target: PathBuf,
        /// Namespace context containing the target.
        context: MountContext,
    },
    /// Persistent mount configuration in a context.
    PersistentMountConfig(MountContext),
}

/// Category of evidence that must be reprobed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    /// Node presence or absence.
    Presence,
    /// Capacity or allocation geometry.
    Geometry,
    /// External identities and labels.
    Identity,
    /// Upper/lower topology relationships.
    Topology,
    /// Content integrity, signatures, or health.
    Content,
    /// Runtime activation state.
    Activation,
    /// Runtime or persistent mount state.
    Mount,
}

/// Evidence that must be observed after backend completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Postcondition {
    /// A node must exist with the predictable requested specification.
    NodeMatches {
        /// Expected node identity.
        id: NodeId,
        /// Expected semantic specification.
        spec: NodeSpec,
    },
    /// A node must no longer be present.
    NodeAbsent(NodeId),
    /// A node must expose the requested capacity.
    NodeSize {
        /// Existing node identity.
        node: NodeId,
        /// Expected capacity.
        size: Bytes,
    },
    /// A partition must expose the requested native attributes.
    PartitionAttributes {
        /// Existing partition identity.
        partition: NodeId,
        /// Expected complete attribute state.
        attributes: storage_core::model::PartitionAttributes,
    },
    /// A filesystem must expose the requested label.
    FilesystemLabel {
        /// Existing filesystem identity.
        filesystem: NodeId,
        /// Expected complete label state.
        label: Option<String>,
    },
    /// Btrfs must expose the requested allocation profiles.
    BtrfsAllocation {
        /// Existing Btrfs filesystem identity.
        filesystem: NodeId,
        /// Expected complete allocation state.
        allocation: BtrfsAllocation,
    },
    /// A dependency edge must be observed.
    DependencyPresent(Dependency),
    /// A dependency edge must be absent.
    DependencyAbsent(Dependency),
    /// Runtime activation must match the requested state.
    Activation {
        /// Existing node identity.
        node: NodeId,
        /// Expected activation state.
        active: bool,
    },
    /// Runtime mount must match the requested source and target.
    RuntimeMountPresent {
        /// Expected mounted source.
        source: MountSource,
        /// Expected target.
        target: PathBuf,
        /// Expected namespace context.
        context: MountContext,
    },
    /// Runtime mount target must be absent.
    RuntimeMountAbsent {
        /// Expected absent target.
        target: PathBuf,
        /// Namespace context checked after execution.
        context: MountContext,
    },
    /// Persistent configuration must contain the requested entry.
    PersistentMountMatches(MountEntry),
    /// Persistent configuration must not contain the target.
    PersistentMountAbsent {
        /// Expected absent target.
        target: PathBuf,
        /// Context owning the persistent configuration.
        context: MountContext,
    },
    /// Provider must successfully re-probe the affected node after an in-place operation.
    NodeReprobed(NodeId),
}

/// Recovery permitted after interruption or an unknown backend result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// Repeating the operation is safe after prerequisites are rechecked.
    Idempotent,
    /// Re-probe state and dispatch only if the effect is still absent.
    ReprobeThenRetry,
    /// Verify postconditions before deciding whether a retry is safe.
    VerifyThenRetry,
    /// Automatic retry is forbidden; explicit recovery is required.
    ManualRecovery,
}

/// Imported contract differs from the trusted recalculation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("imported action contract does not match trusted provider semantics")]
pub struct ActionContractMismatch {
    /// Contract recalculated from the typed action and trusted context.
    pub trusted: Box<ActionContract>,
    /// Untrusted contract supplied by the imported plan.
    pub imported: Box<ActionContract>,
}

/// Computes the only trusted semantic contract for a typed action.
pub fn describe_action(
    action: &ProviderAction,
    context: ActionDescriptionContext<'_>,
) -> ActionContract {
    let mut contract = match action {
        ProviderAction::Partition(action) => describe_partition(action, context),
        ProviderAction::Luks(action) => describe_luks(action, context),
        ProviderAction::Lvm(action) => describe_lvm(action, context),
        ProviderAction::Filesystem(action) => describe_filesystem(action, context),
        ProviderAction::Btrfs(action) => describe_btrfs(action, context),
        ProviderAction::Mount(action) => describe_mount(action),
    };
    contract.provided_outputs = outputs_from_effect(&contract.predicted_effect);
    add_output_resources(&mut contract);
    contract
}

/// Recalculates and verifies an imported informational contract.
///
/// The returned value is always the trusted recalculation, never the imported
/// instance, so callers cannot weaken prerequisites by editing plan JSON.
pub fn verify_imported_contract(
    action: &ProviderAction,
    imported: ActionContract,
    context: ActionDescriptionContext<'_>,
) -> Result<ActionContract, ActionContractMismatch> {
    let trusted = describe_action(action, context);
    if trusted == imported {
        Ok(trusted)
    } else {
        Err(ActionContractMismatch {
            trusted: Box::new(trusted),
            imported: Box::new(imported),
        })
    }
}

/// Creates an empty contract with a selected recovery policy.
fn contract(restart_policy: RestartPolicy) -> ActionContract {
    ActionContract {
        prerequisites: Vec::new(),
        provided_outputs: Vec::new(),
        affected_resources: Vec::new(),
        predicted_effect: PredictedEffect::default(),
        invalidated_facts: Vec::new(),
        postconditions: Vec::new(),
        restart_policy,
    }
}

/// Describes partition actions.
fn describe_partition(
    action: &PartitionAction,
    context: ActionDescriptionContext<'_>,
) -> ActionContract {
    match action {
        PartitionAction::CreateTable {
            disk,
            planned_node_id,
            table,
        } => {
            let mut result = destructive_contract(*disk, RestartPolicy::ManualRecovery);
            let spec = NodeSpec {
                kind: NodeKind::PartitionTable(*table),
                size: known_size(context.graph, *disk),
            };
            let dependency = Dependency {
                from: *disk,
                to: *planned_node_id,
                kind: DependencyKind::Backs,
            };
            result.predicted_effect.graph.extend([
                GraphEffect::InsertNode {
                    id: *planned_node_id,
                    spec: spec.clone(),
                },
                GraphEffect::InsertDependency(dependency.clone()),
            ]);
            result.postconditions.extend([
                Postcondition::NodeMatches {
                    id: *planned_node_id,
                    spec,
                },
                Postcondition::DependencyPresent(dependency),
            ]);
            invalidate(&mut result, *disk, FactKind::Topology);
            result
        }
        PartitionAction::Create {
            table,
            planned_node_id,
            number,
            offset,
            size,
            role,
            attributes,
        } => {
            let mut result = contract(RestartPolicy::ReprobeThenRetry);
            require_kind(&mut result, *table, NodeKindClass::PartitionTable);
            result
                .prerequisites
                .push(ActionPrerequisite::Writable(*table));
            result
                .prerequisites
                .push(ActionPrerequisite::ExtentAvailable {
                    table: *table,
                    offset: *offset,
                    size: *size,
                });
            write(&mut result, ActionResource::PartitionTable(*table));
            let spec = NodeSpec {
                kind: NodeKind::Partition {
                    number: *number,
                    offset: *offset,
                    role: *role,
                    attributes: *attributes,
                },
                size: Some(*size),
            };
            let dependency = Dependency {
                from: *table,
                to: *planned_node_id,
                kind: DependencyKind::Contains { offset: *offset },
            };
            insert_node_with_dependency(&mut result, *planned_node_id, spec, dependency);
            invalidate(&mut result, *table, FactKind::Topology);
            result
        }
        PartitionAction::Resize {
            partition,
            new_size,
        } => resize_contract(*partition, *new_size, NodeKindClass::Partition),
        PartitionAction::Delete { partition } => {
            remove_node_contract(context, *partition, NodeKindClass::Partition, true)
        }
        PartitionAction::SetAttributes {
            partition,
            attributes,
        } => {
            let mut result = contract(RestartPolicy::ReprobeThenRetry);
            require_kind(&mut result, *partition, NodeKindClass::Partition);
            write(&mut result, ActionResource::Node(*partition));
            result
                .predicted_effect
                .graph
                .push(GraphEffect::SetPartitionAttributes {
                    partition: *partition,
                    attributes: *attributes,
                });
            invalidate(&mut result, *partition, FactKind::Identity);
            result
                .postconditions
                .push(Postcondition::PartitionAttributes {
                    partition: *partition,
                    attributes: *attributes,
                });
            result
        }
    }
}

/// Describes LUKS actions.
fn describe_luks(action: &LuksAction, context: ActionDescriptionContext<'_>) -> ActionContract {
    match action {
        LuksAction::Format {
            target,
            planned_node_id,
            version,
            ..
        } => {
            let mut result = destructive_contract(*target, RestartPolicy::VerifyThenRetry);
            let spec = NodeSpec {
                kind: NodeKind::LuksContainer { version: *version },
                size: None,
            };
            let dependency = Dependency {
                from: *target,
                to: *planned_node_id,
                kind: DependencyKind::Backs,
            };
            insert_node_with_dependency(&mut result, *planned_node_id, spec, dependency);
            invalidate(&mut result, *target, FactKind::Content);
            invalidate(&mut result, *target, FactKind::Identity);
            result
        }
        LuksAction::Open {
            container,
            planned_node_id,
            mapping_name,
            ..
        } => {
            let mut result = contract(RestartPolicy::ReprobeThenRetry);
            require_kind(&mut result, *container, NodeKindClass::LuksContainer);
            read(&mut result, ActionResource::Node(*container));
            let spec = NodeSpec {
                kind: NodeKind::DmCryptMapping {
                    name: mapping_name.clone(),
                },
                size: known_size(context.graph, *container),
            };
            let dependency = Dependency {
                from: *container,
                to: *planned_node_id,
                kind: DependencyKind::Backs,
            };
            insert_node_with_dependency(&mut result, *planned_node_id, spec, dependency);
            invalidate(&mut result, *container, FactKind::Topology);
            result
        }
        LuksAction::Close { mapping } => {
            remove_node_contract(context, *mapping, NodeKindClass::DmCryptMapping, true)
        }
        LuksAction::Resize { mapping, new_size } => match new_size {
            Some(size) => resize_contract(*mapping, *size, NodeKindClass::DmCryptMapping),
            None => {
                let mut result = contract(RestartPolicy::ReprobeThenRetry);
                require_kind(&mut result, *mapping, NodeKindClass::DmCryptMapping);
                write(&mut result, ActionResource::Node(*mapping));
                invalidate(&mut result, *mapping, FactKind::Geometry);
                result
                    .postconditions
                    .push(Postcondition::NodeReprobed(*mapping));
                result
            }
        },
    }
}

/// Describes LVM actions.
fn describe_lvm(action: &LvmAction, context: ActionDescriptionContext<'_>) -> ActionContract {
    match action {
        LvmAction::CreatePv {
            backing,
            planned_node_id,
        } => {
            let mut result = destructive_contract(*backing, RestartPolicy::VerifyThenRetry);
            let spec = NodeSpec {
                kind: NodeKind::LvmPv,
                size: None,
            };
            let dependency = Dependency {
                from: *backing,
                to: *planned_node_id,
                kind: DependencyKind::Backs,
            };
            insert_node_with_dependency(&mut result, *planned_node_id, spec, dependency);
            invalidate(&mut result, *backing, FactKind::Content);
            result
        }
        LvmAction::CreateVg {
            planned_node_id,
            name,
            pvs,
        } => {
            let mut result = contract(RestartPolicy::ReprobeThenRetry);
            result
                .prerequisites
                .push(ActionPrerequisite::DistinctNodes(pvs.clone()));
            for pv in pvs {
                require_kind(&mut result, *pv, NodeKindClass::LvmPv);
                write(&mut result, ActionResource::Node(*pv));
            }
            write(&mut result, ActionResource::LvmMetadata(*planned_node_id));
            let spec = NodeSpec {
                kind: NodeKind::LvmVg {
                    name: name.clone(),
                    extent_size: None,
                },
                size: None,
            };
            result.predicted_effect.graph.push(GraphEffect::InsertNode {
                id: *planned_node_id,
                spec: spec.clone(),
            });
            result.postconditions.push(Postcondition::NodeMatches {
                id: *planned_node_id,
                spec,
            });
            for pv in pvs {
                let dependency = Dependency {
                    from: *pv,
                    to: *planned_node_id,
                    kind: DependencyKind::MemberOf(MemberRole::LvmPv),
                };
                add_dependency(&mut result, dependency);
                invalidate(&mut result, *pv, FactKind::Topology);
            }
            result
        }
        LvmAction::CreateLv {
            vg,
            planned_node_id,
            name,
            size,
            layout,
        } => {
            let mut result = contract(RestartPolicy::ReprobeThenRetry);
            require_kind(&mut result, *vg, NodeKindClass::LvmVg);
            write(&mut result, ActionResource::LvmMetadata(*vg));
            let spec = NodeSpec {
                kind: NodeKind::LvmLv(storage_core::model::LvmLv {
                    name: name.clone(),
                    kind: layout.clone(),
                }),
                size: Some(*size),
            };
            let dependency = Dependency {
                from: *vg,
                to: *planned_node_id,
                kind: DependencyKind::Provides,
            };
            insert_node_with_dependency(&mut result, *planned_node_id, spec, dependency);
            invalidate(&mut result, *vg, FactKind::Geometry);
            invalidate(&mut result, *vg, FactKind::Topology);
            result
        }
        LvmAction::ResizeLv { lv, new_size } => {
            let mut result = resize_contract(*lv, *new_size, NodeKindClass::LvmLv);
            write(&mut result, ActionResource::LvmMetadata(*lv));
            result
        }
        LvmAction::RemoveLv { lv } => {
            remove_node_contract(context, *lv, NodeKindClass::LvmLv, true)
        }
        LvmAction::SetActivation { target, active } => {
            let mut result = contract(RestartPolicy::Idempotent);
            result
                .prerequisites
                .push(ActionPrerequisite::NodeExists(*target));
            write(&mut result, ActionResource::Node(*target));
            result
                .predicted_effect
                .graph
                .push(GraphEffect::SetActivation {
                    node: *target,
                    active: *active,
                });
            invalidate(&mut result, *target, FactKind::Activation);
            result.postconditions.push(Postcondition::Activation {
                node: *target,
                active: *active,
            });
            result
        }
    }
}

/// Describes ordinary filesystem actions.
fn describe_filesystem(
    action: &FilesystemAction,
    _context: ActionDescriptionContext<'_>,
) -> ActionContract {
    match action {
        FilesystemAction::Create {
            backing,
            planned_node_id,
            filesystem,
            label,
        } => {
            let mut result = destructive_contract(*backing, RestartPolicy::VerifyThenRetry);
            let spec = NodeSpec {
                kind: NodeKind::Filesystem {
                    kind: filesystem.clone(),
                    label: label.clone(),
                },
                size: None,
            };
            let dependency = Dependency {
                from: *backing,
                to: *planned_node_id,
                kind: DependencyKind::Backs,
            };
            insert_node_with_dependency(&mut result, *planned_node_id, spec, dependency);
            invalidate(&mut result, *backing, FactKind::Content);
            invalidate(&mut result, *backing, FactKind::Identity);
            result
        }
        FilesystemAction::Resize {
            filesystem,
            new_size,
        } => resize_contract(*filesystem, *new_size, NodeKindClass::Filesystem),
        FilesystemAction::Check { filesystem, .. } => inspection_contract(*filesystem),
        FilesystemAction::Repair { filesystem, .. } => {
            let mut result = contract(RestartPolicy::ManualRecovery);
            require_kind(&mut result, *filesystem, NodeKindClass::Filesystem);
            result
                .prerequisites
                .push(ActionPrerequisite::NoUnexpectedConsumers(*filesystem));
            write(&mut result, ActionResource::Node(*filesystem));
            invalidate(&mut result, *filesystem, FactKind::Content);
            result
                .postconditions
                .push(Postcondition::NodeReprobed(*filesystem));
            result
        }
        FilesystemAction::SetLabel { filesystem, label } => {
            let mut result = contract(RestartPolicy::ReprobeThenRetry);
            require_kind(&mut result, *filesystem, NodeKindClass::Filesystem);
            write(&mut result, ActionResource::Node(*filesystem));
            result
                .predicted_effect
                .graph
                .push(GraphEffect::SetFilesystemLabel {
                    filesystem: *filesystem,
                    label: label.clone(),
                });
            invalidate(&mut result, *filesystem, FactKind::Identity);
            result.postconditions.push(Postcondition::FilesystemLabel {
                filesystem: *filesystem,
                label: label.clone(),
            });
            result
        }
    }
}

/// Describes Btrfs actions.
fn describe_btrfs(action: &BtrfsAction, context: ActionDescriptionContext<'_>) -> ActionContract {
    match action {
        BtrfsAction::Create {
            members,
            planned_node_id,
            label,
            allocation,
        } => {
            let mut result = contract(RestartPolicy::VerifyThenRetry);
            result
                .prerequisites
                .push(ActionPrerequisite::DistinctNodes(members.clone()));
            for member in members {
                result.prerequisites.extend([
                    ActionPrerequisite::NodeExists(*member),
                    ActionPrerequisite::NodeKind {
                        node: *member,
                        expected: NodeKindClass::Block,
                    },
                    ActionPrerequisite::Writable(*member),
                    ActionPrerequisite::NoUnexpectedConsumers(*member),
                    ActionPrerequisite::DestructiveApproval(*member),
                ]);
                write(&mut result, ActionResource::Node(*member));
                invalidate(&mut result, *member, FactKind::Content);
            }
            let spec = NodeSpec {
                kind: NodeKind::Filesystem {
                    kind: FilesystemKind::Btrfs(allocation.clone()),
                    label: label.clone(),
                },
                size: None,
            };
            result.predicted_effect.graph.push(GraphEffect::InsertNode {
                id: *planned_node_id,
                spec: spec.clone(),
            });
            result.postconditions.push(Postcondition::NodeMatches {
                id: *planned_node_id,
                spec,
            });
            for member in members {
                add_dependency(
                    &mut result,
                    Dependency {
                        from: *member,
                        to: *planned_node_id,
                        kind: DependencyKind::MemberOf(MemberRole::Btrfs(BtrfsMember {
                            devid: None,
                        })),
                    },
                );
            }
            result
        }
        BtrfsAction::AddDevice { filesystem, device } => {
            let mut result = btrfs_membership_contract(*filesystem, *device);
            add_dependency(
                &mut result,
                Dependency {
                    from: *device,
                    to: *filesystem,
                    kind: DependencyKind::MemberOf(MemberRole::Btrfs(BtrfsMember { devid: None })),
                },
            );
            result
        }
        BtrfsAction::RemoveDevice { filesystem, device } => {
            let mut result = btrfs_membership_contract(*filesystem, *device);
            let dependency = context
                .graph
                .dependencies()
                .find(|dependency| {
                    dependency.from == *device
                        && dependency.to == *filesystem
                        && matches!(
                            dependency.kind,
                            DependencyKind::MemberOf(MemberRole::Btrfs(_))
                        )
                })
                .cloned()
                .unwrap_or(Dependency {
                    from: *device,
                    to: *filesystem,
                    kind: DependencyKind::MemberOf(MemberRole::Btrfs(BtrfsMember { devid: None })),
                });
            result
                .predicted_effect
                .graph
                .push(GraphEffect::RemoveDependency(dependency.clone()));
            result
                .postconditions
                .push(Postcondition::DependencyAbsent(dependency));
            result
        }
        BtrfsAction::ConvertProfiles { filesystem, target } => {
            let mut result = contract(RestartPolicy::ReprobeThenRetry);
            require_kind(&mut result, *filesystem, NodeKindClass::BtrfsFilesystem);
            write(&mut result, ActionResource::Node(*filesystem));
            result
                .predicted_effect
                .graph
                .push(GraphEffect::SetBtrfsAllocation {
                    filesystem: *filesystem,
                    allocation: target.clone(),
                });
            invalidate(&mut result, *filesystem, FactKind::Geometry);
            result.postconditions.push(Postcondition::BtrfsAllocation {
                filesystem: *filesystem,
                allocation: target.clone(),
            });
            result
        }
        BtrfsAction::CreateSubvolume {
            filesystem,
            planned_node_id,
            name,
            read_only,
        } => {
            let mut result = contract(RestartPolicy::ReprobeThenRetry);
            require_kind(&mut result, *filesystem, NodeKindClass::BtrfsFilesystem);
            write(&mut result, ActionResource::Node(*filesystem));
            let spec = NodeSpec {
                kind: NodeKind::BtrfsSubvolume {
                    name: name.clone(),
                    read_only: *read_only,
                    is_default: false,
                },
                size: None,
            };
            let dependency = Dependency {
                from: *filesystem,
                to: *planned_node_id,
                kind: DependencyKind::Provides,
            };
            insert_node_with_dependency(&mut result, *planned_node_id, spec, dependency);
            invalidate(&mut result, *filesystem, FactKind::Topology);
            result
        }
    }
}

/// Describes mount actions.
fn describe_mount(action: &MountAction) -> ActionContract {
    match action {
        MountAction::Mount {
            source,
            target,
            options,
            context,
        } => {
            let mut result = contract(RestartPolicy::Idempotent);
            if let Some(node) = mount_source_node(source) {
                result
                    .prerequisites
                    .push(ActionPrerequisite::NodeExists(node));
                read(&mut result, ActionResource::Node(node));
            }
            result
                .prerequisites
                .push(ActionPrerequisite::MountTargetAvailable {
                    target: target.clone(),
                    context: context.clone(),
                });
            write(
                &mut result,
                ActionResource::MountTarget {
                    target: target.clone(),
                    context: context.clone(),
                },
            );
            result
                .predicted_effect
                .mounts
                .push(MountEffect::AddRuntime {
                    source: source.clone(),
                    target: target.clone(),
                    options: options.clone(),
                    context: context.clone(),
                });
            invalidate_mount(&mut result, target, context);
            result
                .postconditions
                .push(Postcondition::RuntimeMountPresent {
                    source: source.clone(),
                    target: target.clone(),
                    context: context.clone(),
                });
            result
        }
        MountAction::Unmount { target, context } => {
            let mut result = contract(RestartPolicy::Idempotent);
            write(
                &mut result,
                ActionResource::MountTarget {
                    target: target.clone(),
                    context: context.clone(),
                },
            );
            result
                .predicted_effect
                .mounts
                .push(MountEffect::RemoveRuntime {
                    target: target.clone(),
                    context: context.clone(),
                });
            invalidate_mount(&mut result, target, context);
            result
                .postconditions
                .push(Postcondition::RuntimeMountAbsent {
                    target: target.clone(),
                    context: context.clone(),
                });
            result
        }
        MountAction::Persist { entry } => {
            let mut result = contract(RestartPolicy::ReprobeThenRetry);
            write(
                &mut result,
                ActionResource::PersistentMountConfig(entry.context.clone()),
            );
            result
                .predicted_effect
                .mounts
                .push(MountEffect::SetPersistent(entry.clone()));
            invalidate_persistent_mount(&mut result, entry.context.clone());
            result
                .postconditions
                .push(Postcondition::PersistentMountMatches(entry.clone()));
            result
        }
        MountAction::RemovePersistent { target, context } => {
            let mut result = contract(RestartPolicy::ReprobeThenRetry);
            write(
                &mut result,
                ActionResource::PersistentMountConfig(context.clone()),
            );
            result
                .predicted_effect
                .mounts
                .push(MountEffect::RemovePersistent {
                    target: target.clone(),
                    context: context.clone(),
                });
            invalidate_persistent_mount(&mut result, context.clone());
            result
                .postconditions
                .push(Postcondition::PersistentMountAbsent {
                    target: target.clone(),
                    context: context.clone(),
                });
            result
        }
    }
}

/// Builds a contract for destructive content replacement.
fn destructive_contract(node: NodeId, restart_policy: RestartPolicy) -> ActionContract {
    let mut result = contract(restart_policy);
    result.prerequisites.extend([
        ActionPrerequisite::NodeExists(node),
        ActionPrerequisite::NodeKind {
            node,
            expected: NodeKindClass::Block,
        },
        ActionPrerequisite::Writable(node),
        ActionPrerequisite::NoUnexpectedConsumers(node),
        ActionPrerequisite::DestructiveApproval(node),
    ]);
    write(&mut result, ActionResource::Node(node));
    result
}

/// Builds a contract for a predictable size change.
fn resize_contract(node: NodeId, size: Bytes, expected: NodeKindClass) -> ActionContract {
    let mut result = contract(RestartPolicy::ReprobeThenRetry);
    require_kind(&mut result, node, expected);
    result
        .prerequisites
        .push(ActionPrerequisite::NoUnexpectedConsumers(node));
    write(&mut result, ActionResource::Node(node));
    result
        .predicted_effect
        .graph
        .push(GraphEffect::SetNodeSize { node, size });
    invalidate(&mut result, node, FactKind::Geometry);
    result
        .postconditions
        .push(Postcondition::NodeSize { node, size });
    result
}

/// Builds a contract for removing a graph node.
fn remove_node_contract(
    context: ActionDescriptionContext<'_>,
    node: NodeId,
    expected: NodeKindClass,
    require_no_consumers: bool,
) -> ActionContract {
    let mut result = contract(RestartPolicy::ReprobeThenRetry);
    require_kind(&mut result, node, expected);
    if require_no_consumers {
        result
            .prerequisites
            .push(ActionPrerequisite::NoUnexpectedConsumers(node));
    }
    write(&mut result, ActionResource::Node(node));
    result
        .predicted_effect
        .graph
        .push(GraphEffect::RemoveNode(node));
    for dependency in context
        .graph
        .dependencies()
        .filter(|dependency| dependency.from == node || dependency.to == node)
    {
        result
            .predicted_effect
            .graph
            .push(GraphEffect::RemoveDependency(dependency.clone()));
        result
            .postconditions
            .push(Postcondition::DependencyAbsent(dependency.clone()));
    }
    for relation in context
        .graph
        .relations()
        .filter(|relation| relation.from == node || relation.to == node)
    {
        result
            .predicted_effect
            .graph
            .push(GraphEffect::RemoveRelation(relation.clone()));
    }
    invalidate(&mut result, node, FactKind::Presence);
    result.postconditions.push(Postcondition::NodeAbsent(node));
    result
}

/// Builds a read-only filesystem inspection contract.
fn inspection_contract(node: NodeId) -> ActionContract {
    let mut result = contract(RestartPolicy::Idempotent);
    require_kind(&mut result, node, NodeKindClass::Filesystem);
    read(&mut result, ActionResource::Node(node));
    invalidate(&mut result, node, FactKind::Content);
    result
        .postconditions
        .push(Postcondition::NodeReprobed(node));
    result
}

/// Builds common Btrfs membership prerequisites and resource locks.
fn btrfs_membership_contract(filesystem: NodeId, device: NodeId) -> ActionContract {
    let mut result = contract(RestartPolicy::ReprobeThenRetry);
    require_kind(&mut result, filesystem, NodeKindClass::BtrfsFilesystem);
    require_kind(&mut result, device, NodeKindClass::Block);
    write(&mut result, ActionResource::Node(filesystem));
    write(&mut result, ActionResource::Node(device));
    invalidate(&mut result, filesystem, FactKind::Topology);
    invalidate(&mut result, device, FactKind::Topology);
    result
}

/// Adds one predictable node and its backing/membership edge.
fn insert_node_with_dependency(
    result: &mut ActionContract,
    id: NodeId,
    spec: NodeSpec,
    dependency: Dependency,
) {
    result.predicted_effect.graph.extend([
        GraphEffect::InsertNode {
            id,
            spec: spec.clone(),
        },
        GraphEffect::InsertDependency(dependency.clone()),
    ]);
    result.postconditions.extend([
        Postcondition::NodeMatches { id, spec },
        Postcondition::DependencyPresent(dependency),
    ]);
}

/// Adds one dependency effect and matching postcondition.
fn add_dependency(result: &mut ActionContract, dependency: Dependency) {
    result
        .predicted_effect
        .graph
        .push(GraphEffect::InsertDependency(dependency.clone()));
    result
        .postconditions
        .push(Postcondition::DependencyPresent(dependency));
}

/// Adds node-kind prerequisites.
fn require_kind(result: &mut ActionContract, node: NodeId, expected: NodeKindClass) {
    result.prerequisites.extend([
        ActionPrerequisite::NodeExists(node),
        ActionPrerequisite::NodeKind { node, expected },
    ]);
}

/// Records read access without deduplicating intentional declarations.
fn read(result: &mut ActionContract, resource: ActionResource) {
    result.affected_resources.push(ResourceAccess {
        resource,
        mode: ResourceAccessMode::Read,
    });
}

/// Records write access without hiding ordering-relevant declarations.
fn write(result: &mut ActionContract, resource: ActionResource) {
    result.affected_resources.push(ResourceAccess {
        resource,
        mode: ResourceAccessMode::Write,
    });
}

/// Invalidates one category of node evidence.
fn invalidate(result: &mut ActionContract, node: NodeId, kind: FactKind) {
    result.invalidated_facts.push(FactInvalidation {
        scope: FactScope::Node(node),
        kind,
    });
}

/// Invalidates one runtime mount target.
fn invalidate_mount(result: &mut ActionContract, target: &Path, context: &MountContext) {
    result.invalidated_facts.push(FactInvalidation {
        scope: FactScope::MountTarget {
            target: target.to_path_buf(),
            context: context.clone(),
        },
        kind: FactKind::Mount,
    });
}

/// Invalidates persistent mount configuration in one context.
fn invalidate_persistent_mount(result: &mut ActionContract, context: MountContext) {
    result.invalidated_facts.push(FactInvalidation {
        scope: FactScope::PersistentMountConfig(context),
        kind: FactKind::Mount,
    });
}

/// Resolves predictable current capacity without inventing future facts.
fn known_size(graph: &NodeGraph, node: NodeId) -> Option<Bytes> {
    graph.node(&node).and_then(|node| node.kind.size)
}

/// Returns the graph node referenced by a mount source, when one exists.
fn mount_source_node(source: &MountSource) -> Option<NodeId> {
    match source {
        MountSource::Filesystem(node)
        | MountSource::BtrfsSubvolume(node)
        | MountSource::ZfsDataset(node) => Some(*node),
        MountSource::Bind(_)
        | MountSource::Tmpfs
        | MountSource::Network(_)
        | MountSource::Other(_) => None,
    }
}

/// Derives declared outputs directly from the shared predicted effect.
fn outputs_from_effect(effect: &PredictedEffect) -> Vec<ActionOutput> {
    let mut outputs = effect
        .graph
        .iter()
        .filter_map(|change| match change {
            GraphEffect::InsertNode { id, spec } => Some(ActionOutput::Node {
                id: *id,
                spec: spec.clone(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    outputs.extend(effect.mounts.iter().filter_map(|change| match change {
        MountEffect::AddRuntime {
            source,
            target,
            context,
            ..
        } => Some(ActionOutput::Mount {
            source: source.clone(),
            target: target.clone(),
            context: context.clone(),
        }),
        MountEffect::SetPersistent(entry) => Some(ActionOutput::PersistentMount(entry.clone())),
        MountEffect::RemoveRuntime { .. } | MountEffect::RemovePersistent { .. } => None,
    }));
    outputs
}

/// Adds planned outputs to the affected-resource set used for locking.
fn add_output_resources(contract: &mut ActionContract) {
    let output_nodes = contract
        .provided_outputs
        .iter()
        .filter_map(|output| match output {
            ActionOutput::Node { id, .. } => Some(*id),
            ActionOutput::Mount { .. } | ActionOutput::PersistentMount(_) => None,
        })
        .collect::<Vec<_>>();
    for node in output_nodes {
        let access = ResourceAccess {
            resource: ActionResource::Node(node),
            mode: ResourceAccessMode::Write,
        };
        if !contract.affected_resources.contains(&access) {
            contract.affected_resources.push(access);
        }
    }
}

#[cfg(test)]
mod tests {
    use storage_core::model::{Node, NodeFacts, Presence};

    use super::*;

    /// Creates a graph containing one predictable block node.
    fn block_graph() -> (NodeGraph, NodeId) {
        let backing = NodeId::new();
        let mut graph = NodeGraph::new();
        graph.insert_node(
            backing,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::Disk,
                    size: Some(Bytes::new(1_000_000)),
                },
                size: NodeFacts {
                    observed_in: None,
                    presence: Presence::Present,
                    identities: Vec::new(),
                    block: None,
                    device: None,
                },
            },
        );
        (graph, backing)
    }

    /// Ensures one description owns prerequisites, simulation, and verification evidence.
    #[test]
    fn filesystem_create_contract_is_complete_and_serializable() {
        let (graph, backing) = block_graph();
        let planned_node_id = NodeId::new();
        let action = ProviderAction::Filesystem(FilesystemAction::Create {
            backing,
            planned_node_id,
            filesystem: FilesystemKind::Ext4,
            label: Some("root".to_owned()),
        });
        let contract = describe_action(&action, ActionDescriptionContext { graph: &graph });
        let json = serde_json::to_string(&contract)
            .unwrap_or_else(|error| panic!("serialize action contract: {error}"));
        let restored: ActionContract = serde_json::from_str(&json)
            .unwrap_or_else(|error| panic!("deserialize action contract: {error}"));

        assert_eq!(restored, contract);
        assert!(
            contract
                .prerequisites
                .contains(&ActionPrerequisite::NoUnexpectedConsumers(backing))
        );
        assert!(
            contract
                .prerequisites
                .contains(&ActionPrerequisite::DestructiveApproval(backing))
        );
        assert!(matches!(
            contract.provided_outputs.as_slice(),
            [ActionOutput::Node { id, .. }] if *id == planned_node_id
        ));
        assert!(contract.predicted_effect.graph.iter().any(
            |effect| matches!(effect, GraphEffect::InsertNode { id, .. } if *id == planned_node_id)
        ));
    }

    /// Ensures imported JSON cannot remove a mandatory safety prerequisite.
    #[test]
    fn imported_contract_cannot_weaken_trusted_description() {
        let (graph, backing) = block_graph();
        let action = ProviderAction::Filesystem(FilesystemAction::Create {
            backing,
            planned_node_id: NodeId::new(),
            filesystem: FilesystemKind::Ext4,
            label: None,
        });
        let context = ActionDescriptionContext { graph: &graph };
        let mut imported = describe_action(&action, context);
        imported.prerequisites.retain(|requirement| {
            !matches!(requirement, ActionPrerequisite::NoUnexpectedConsumers(_))
        });

        let mismatch = match verify_imported_contract(&action, imported, context) {
            Ok(_) => panic!("weakened contract must be rejected"),
            Err(error) => error,
        };
        assert!(
            mismatch
                .trusted
                .prerequisites
                .contains(&ActionPrerequisite::NoUnexpectedConsumers(backing))
        );
    }

    /// Ensures trusted recalculation, not the imported allocation, is returned.
    #[test]
    fn matching_import_returns_recalculated_contract() {
        let (graph, backing) = block_graph();
        let action = ProviderAction::Luks(LuksAction::Format {
            target: backing,
            planned_node_id: NodeId::new(),
            version: storage_core::model::LuksVersion::Luks2,
            credential: crate::CredentialRef::from_uuid(uuid::Uuid::nil()),
        });
        let context = ActionDescriptionContext { graph: &graph };
        let imported = describe_action(&action, context);

        let trusted = verify_imported_contract(&action, imported.clone(), context)
            .unwrap_or_else(|error| panic!("verify matching contract: {error}"));
        assert_eq!(trusted, imported);
    }
}
