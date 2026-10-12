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
    /// The filesystem must be unmounted in every observed namespace.
    FilesystemUnmounted(NodeId),
    /// A preceding forced read-only check must verify the filesystem is clean.
    FilesystemCheckPassed(NodeId),
    /// The requested shrink target must fit the provider's safe minimum size.
    FilesystemMinimumSize {
        /// Filesystem whose content must fit.
        filesystem: NodeId,
        /// Requested resulting capacity.
        target: Bytes,
    },
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
    /// Every directly backed upper layer must fit within a new capacity.
    DependentsFitWithin {
        /// Lower-layer node whose exposed capacity will change.
        node: NodeId,
        /// Capacity that remains available after the action.
        size: Bytes,
    },
    /// The direct backing layer must provide at least the requested capacity.
    BackingSizeAtLeast {
        /// Upper-layer node whose capacity will change.
        node: NodeId,
        /// Capacity required from its direct backing node.
        size: Bytes,
    },
    /// A planning allocation domain must retain enough free capacity.
    CapacityAvailable {
        /// Provider-owned allocation domain.
        domain: AllocationDomain,
        /// Provider-calculated units consumed by this action, when known.
        amount: Option<AllocationQuantity>,
    },
    /// A mount target must not already be occupied in the selected context.
    MountTargetAvailable {
        /// Requested target path.
        target: PathBuf,
        /// Namespace context containing the path.
        context: MountContext,
    },
    /// A parent mount change must not hide or detach active child mounts.
    MountDescendantsAbsent {
        /// Parent mount target being changed.
        target: PathBuf,
        /// Namespace in which descendants must be absent.
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
    /// Runtime block interface made available for dependent actions.
    BlockInterface {
        /// Node exposing the interface.
        node: NodeId,
        /// Access mode guaranteed after successful verification.
        access: BlockInterfaceAccess,
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
    /// A forced read-only check found no errors after its postcondition was verified.
    FilesystemCheckPassed(NodeId),
}

/// Access mode guaranteed by a projected runtime block interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockInterfaceAccess {
    /// Consumers may inspect but must not modify the endpoint.
    ReadOnly,
    /// Consumers may read and write the endpoint.
    ReadWrite,
}

/// Provider-owned domain whose free capacity is tracked during planning.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationDomain {
    /// Allocatable extents belonging to one LVM volume group.
    LvmVolumeGroup(NodeId),
}

/// Count of provider-defined allocation units within one domain.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AllocationQuantity(u64);

impl AllocationQuantity {
    /// Creates an allocation-unit count.
    pub const fn new(units: u64) -> Self {
        Self(units)
    }

    /// Returns the provider-defined unit count.
    pub const fn get(self) -> u64 {
        self.0
    }
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
    /// Runtime mounts across namespaces, shared by mount changes and offline filesystem work.
    /// This conservative domain prevents reordering until finer mount ownership is available.
    RuntimeMountState,
    /// Canonical storage node.
    Node(NodeId),
    /// Partition-table metadata domain keyed by the canonical table node, never a partition.
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
    /// Changes to planning-only allocation accounting.
    #[serde(default)]
    pub allocations: Vec<AllocationEffect>,
}

/// One predicted change to planning-only resource accounting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationEffect {
    /// Discards free-capacity evidence after a mutation with unknown physical allocation.
    Invalidate {
        /// Domain that needs fresh provider allocation evidence.
        domain: AllocationDomain,
    },
    /// Consumes capacity from a provider-owned allocation domain.
    Reserve {
        /// Domain whose free capacity decreases.
        domain: AllocationDomain,
        /// Capacity consumed after successful completion.
        amount: AllocationQuantity,
    },
    /// Returns capacity to a provider-owned allocation domain.
    Release {
        /// Domain whose free capacity increases.
        domain: AllocationDomain,
        /// Capacity returned after successful completion.
        amount: AllocationQuantity,
    },
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
    /// A forced read-only filesystem check completed and found no errors.
    /// Finding errors fails this postcondition even if the checker finished normally.
    FilesystemCheckPassed(NodeId),
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
    let mut outputs = outputs_from_effect(&contract.predicted_effect);
    outputs.append(&mut contract.provided_outputs);
    contract.provided_outputs = outputs;
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
    let mut result = match action {
        PartitionAction::CreateTable {
            disk,
            planned_node_id,
            table,
        } => {
            let mut result = destructive_contract(*disk, RestartPolicy::ManualRecovery);
            write(
                &mut result,
                ActionResource::PartitionTable(*planned_node_id),
            );
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
        } => resize_contract(
            context,
            *partition,
            *new_size,
            NodeKindClass::Partition,
            ResizeSafety::DEPENDENTS,
        ),
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
    };
    match action {
        PartitionAction::Resize { partition, .. }
        | PartitionAction::Delete { partition }
        | PartitionAction::SetAttributes { partition, .. } => {
            // Technology validation rejects missing or ambiguous ownership before simulation.
            if let Some(table) = crate::logical::partition_table(context.graph, *partition) {
                write(&mut result, ActionResource::PartitionTable(table));
                invalidate(&mut result, table, FactKind::Topology);
            }
        }
        PartitionAction::CreateTable { .. } | PartitionAction::Create { .. } => {}
    }
    result
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
            read_only,
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
            result.provided_outputs.push(ActionOutput::BlockInterface {
                node: *planned_node_id,
                access: if *read_only {
                    BlockInterfaceAccess::ReadOnly
                } else {
                    BlockInterfaceAccess::ReadWrite
                },
            });
            invalidate(&mut result, *container, FactKind::Topology);
            result
        }
        LuksAction::Close { mapping } => {
            remove_node_contract(context, *mapping, NodeKindClass::DmCryptMapping, true)
        }
        LuksAction::Resize { mapping, new_size } => match new_size {
            Some(size) => resize_contract(
                context,
                *mapping,
                *size,
                NodeKindClass::DmCryptMapping,
                ResizeSafety::BOTH,
            ),
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
            let allocation_domain = AllocationDomain::LvmVolumeGroup(*vg);
            let allocation = lvm_allocation(context.graph, *vg, *size, layout);
            result
                .prerequisites
                .push(ActionPrerequisite::CapacityAvailable {
                    domain: allocation_domain.clone(),
                    amount: allocation,
                });
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
            if let Some(amount) = allocation {
                result
                    .predicted_effect
                    .allocations
                    .push(AllocationEffect::Reserve {
                        domain: allocation_domain,
                        amount,
                    });
            } else {
                result
                    .predicted_effect
                    .allocations
                    .push(AllocationEffect::Invalidate {
                        domain: allocation_domain,
                    });
            }
            invalidate(&mut result, *vg, FactKind::Geometry);
            invalidate(&mut result, *vg, FactKind::Topology);
            result
        }
        LvmAction::ResizeLv { lv, new_size } => {
            let mut result = resize_contract(
                context,
                *lv,
                *new_size,
                NodeKindClass::LvmLv,
                ResizeSafety::DEPENDENTS,
            );
            if let Some(vg) = lvm_volume_group(context.graph, *lv) {
                write(&mut result, ActionResource::LvmMetadata(vg));
                let domain = AllocationDomain::LvmVolumeGroup(vg);
                let old = known_size(context.graph, *lv)
                    .and_then(|size| lvm_lv_allocation(context.graph, vg, *lv, size));
                let new = lvm_lv_allocation(context.graph, vg, *lv, *new_size);
                match (old, new) {
                    (Some(old), Some(new)) if new > old => {
                        let amount = AllocationQuantity::new(new.get() - old.get());
                        result
                            .prerequisites
                            .push(ActionPrerequisite::CapacityAvailable {
                                domain: domain.clone(),
                                amount: Some(amount),
                            });
                        result
                            .predicted_effect
                            .allocations
                            .push(AllocationEffect::Reserve { domain, amount });
                    }
                    (Some(old), Some(new)) if new < old => {
                        result
                            .predicted_effect
                            .allocations
                            .push(AllocationEffect::Release {
                                domain,
                                amount: AllocationQuantity::new(old.get() - new.get()),
                            });
                    }
                    (Some(_), Some(_)) => {}
                    _ => {
                        result
                            .prerequisites
                            .push(ActionPrerequisite::CapacityAvailable {
                                domain: domain.clone(),
                                amount: None,
                            });
                        result
                            .predicted_effect
                            .allocations
                            .push(AllocationEffect::Invalidate { domain });
                    }
                }
                invalidate(&mut result, vg, FactKind::Geometry);
            }
            result
        }
        LvmAction::RemoveLv { lv } => {
            let mut result = remove_node_contract(context, *lv, NodeKindClass::LvmLv, true);
            if let Some(vg) = lvm_volume_group(context.graph, *lv) {
                write(&mut result, ActionResource::LvmMetadata(vg));
                let domain = AllocationDomain::LvmVolumeGroup(vg);
                let allocation = known_size(context.graph, *lv)
                    .and_then(|size| lvm_lv_allocation(context.graph, vg, *lv, size));
                result.predicted_effect.allocations.push(match allocation {
                    Some(amount) => AllocationEffect::Release { domain, amount },
                    None => AllocationEffect::Invalidate { domain },
                });
                invalidate(&mut result, vg, FactKind::Geometry);
                invalidate(&mut result, vg, FactKind::Topology);
            }
            result
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
            if *active
                && context
                    .graph
                    .node(target)
                    .is_some_and(|node| matches!(node.kind.kind, NodeKind::LvmLv(_)))
            {
                result.provided_outputs.push(ActionOutput::BlockInterface {
                    node: *target,
                    access: BlockInterfaceAccess::ReadWrite,
                });
            }
            result
        }
    }
}

/// Describes ordinary filesystem actions.
fn describe_filesystem(
    action: &FilesystemAction,
    context: ActionDescriptionContext<'_>,
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
        } => {
            let mut result = resize_contract(
                context,
                *filesystem,
                *new_size,
                NodeKindClass::Filesystem,
                ResizeSafety::BACKING,
            );
            let may_shrink =
                known_size(context.graph, *filesystem).is_none_or(|current| current > *new_size);
            let is_ext2 = context.graph.node(filesystem).is_some_and(|node| {
                matches!(
                    node.kind.kind,
                    NodeKind::Filesystem {
                        kind: FilesystemKind::Ext2,
                        ..
                    }
                )
            });
            if may_shrink || is_ext2 {
                result
                    .prerequisites
                    .push(ActionPrerequisite::FilesystemUnmounted(*filesystem));
                read(&mut result, ActionResource::RuntimeMountState);
            }
            if may_shrink {
                result.prerequisites.extend([
                    ActionPrerequisite::FilesystemCheckPassed(*filesystem),
                    ActionPrerequisite::FilesystemMinimumSize {
                        filesystem: *filesystem,
                        target: *new_size,
                    },
                ]);
            }
            access_filesystem_backing(
                &mut result,
                context.graph,
                *filesystem,
                ResourceAccessMode::Write,
            );
            result
        }
        FilesystemAction::Check { filesystem, force } => {
            let mut result = inspection_contract(context.graph, *filesystem);
            if *force && is_ext_filesystem(context.graph, *filesystem) {
                result
                    .provided_outputs
                    .push(ActionOutput::FilesystemCheckPassed(*filesystem));
                result
                    .postconditions
                    .push(Postcondition::FilesystemCheckPassed(*filesystem));
            }
            result
        }
        FilesystemAction::Repair { filesystem, .. } => {
            let mut result = contract(RestartPolicy::ManualRecovery);
            require_kind(&mut result, *filesystem, NodeKindClass::Filesystem);
            result
                .prerequisites
                .push(ActionPrerequisite::FilesystemUnmounted(*filesystem));
            read(&mut result, ActionResource::RuntimeMountState);
            result
                .prerequisites
                .push(ActionPrerequisite::NoUnexpectedConsumers(*filesystem));
            write(&mut result, ActionResource::Node(*filesystem));
            access_filesystem_backing(
                &mut result,
                context.graph,
                *filesystem,
                ResourceAccessMode::Write,
            );
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
            write(&mut result, ActionResource::RuntimeMountState);
            result
                .prerequisites
                .push(ActionPrerequisite::MountDescendantsAbsent {
                    target: target.clone(),
                    context: context.clone(),
                });
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
            write(&mut result, ActionResource::RuntimeMountState);
            result
                .prerequisites
                .push(ActionPrerequisite::MountDescendantsAbsent {
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
fn resize_contract(
    context: ActionDescriptionContext<'_>,
    node: NodeId,
    size: Bytes,
    expected: NodeKindClass,
    safety: ResizeSafety,
) -> ActionContract {
    let mut result = contract(RestartPolicy::ReprobeThenRetry);
    require_kind(&mut result, node, expected);
    let current_size = known_size(context.graph, node);
    if safety.dependents && current_size.is_none_or(|current| current > size) {
        result
            .prerequisites
            .push(ActionPrerequisite::DependentsFitWithin { node, size });
    }
    if safety.backing && current_size.is_none_or(|current| current < size) {
        result
            .prerequisites
            .push(ActionPrerequisite::BackingSizeAtLeast { node, size });
    }
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

/// Capacity relationships that must hold around one resize action.
#[derive(Debug, Clone, Copy)]
struct ResizeSafety {
    dependents: bool,
    backing: bool,
}

impl ResizeSafety {
    /// Checks only upper layers that consume this node.
    const DEPENDENTS: Self = Self {
        dependents: true,
        backing: false,
    };

    /// Checks only the lower layer that backs this node.
    const BACKING: Self = Self {
        dependents: false,
        backing: true,
    };

    /// Checks capacity on both sides of this node.
    const BOTH: Self = Self {
        dependents: true,
        backing: true,
    };
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
fn inspection_contract(graph: &NodeGraph, node: NodeId) -> ActionContract {
    let mut result = contract(RestartPolicy::Idempotent);
    require_kind(&mut result, node, NodeKindClass::Filesystem);
    result
        .prerequisites
        .push(ActionPrerequisite::FilesystemUnmounted(node));
    read(&mut result, ActionResource::RuntimeMountState);
    read(&mut result, ActionResource::Node(node));
    access_filesystem_backing(&mut result, graph, node, ResourceAccessMode::Read);
    invalidate(&mut result, node, FactKind::Content);
    result
        .postconditions
        .push(Postcondition::NodeReprobed(node));
    result
}

/// Locks the direct backing endpoint and requires write access for filesystem mutations.
fn access_filesystem_backing(
    result: &mut ActionContract,
    graph: &NodeGraph,
    filesystem: NodeId,
    mode: ResourceAccessMode,
) {
    for dependency in graph.dependencies().filter(|dependency| {
        dependency.to == filesystem && matches!(dependency.kind, DependencyKind::Backs)
    }) {
        result.affected_resources.push(ResourceAccess {
            resource: ActionResource::Node(dependency.from),
            mode,
        });
        if mode == ResourceAccessMode::Write {
            result
                .prerequisites
                .push(ActionPrerequisite::Writable(dependency.from));
        }
    }
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

/// Identifies the filesystem family covered by the current resize and check recipe.
pub(crate) fn is_ext_filesystem(graph: &NodeGraph, node: NodeId) -> bool {
    graph.node(&node).is_some_and(|node| {
        matches!(
            node.kind.kind,
            NodeKind::Filesystem {
                kind: FilesystemKind::Ext2 | FilesystemKind::Ext3 | FilesystemKind::Ext4,
                ..
            }
        )
    })
}

/// Calculates exact linear-LV allocation only for known, extent-aligned capacity.
fn lvm_allocation(
    graph: &NodeGraph,
    vg: NodeId,
    size: Bytes,
    layout: &storage_core::model::LvmLvKind,
) -> Option<AllocationQuantity> {
    // shortcut: non-linear LVM layouts need provider-specific placement and redundancy accounting.
    if !matches!(layout, storage_core::model::LvmLvKind::Linear) {
        return None;
    }
    let extent_size = match &graph.node(&vg)?.kind.kind {
        NodeKind::LvmVg {
            extent_size: Some(extent_size),
            ..
        } if extent_size.as_u64() > 0 => extent_size.as_u64(),
        _ => return None,
    };
    if size.as_u64() == 0 || !size.as_u64().is_multiple_of(extent_size) {
        return None;
    }
    Some(AllocationQuantity::new(size.as_u64() / extent_size))
}

/// Calculates allocation for the layout of an existing or previously created LV.
fn lvm_lv_allocation(
    graph: &NodeGraph,
    vg: NodeId,
    lv: NodeId,
    size: Bytes,
) -> Option<AllocationQuantity> {
    let NodeKind::LvmLv(value) = &graph.node(&lv)?.kind.kind else {
        return None;
    };
    lvm_allocation(graph, vg, size, &value.kind)
}

/// Returns exactly one owning VG metadata domain, rejecting missing or ambiguous ownership.
pub(crate) fn lvm_volume_group(graph: &NodeGraph, lv: NodeId) -> Option<NodeId> {
    let mut owners = graph
        .dependencies()
        .filter(|dependency| dependency.to == lv && dependency.kind == DependencyKind::Provides);
    let owner = owners.next()?;
    (owners.next().is_none()
        && graph
            .node(&owner.from)
            .is_some_and(|node| matches!(node.kind.kind, NodeKind::LvmVg { .. })))
    .then_some(owner.from)
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
            ActionOutput::Node { id, .. } | ActionOutput::BlockInterface { node: id, .. } => {
                Some(*id)
            }
            ActionOutput::Mount { .. }
            | ActionOutput::PersistentMount(_)
            | ActionOutput::FilesystemCheckPassed(_) => None,
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
    use storage_core::model::{
        Dependency, DependencyKind, LuksVersion, LvmLv, LvmLvKind, Node, NodeFacts, Presence,
    };

    use super::*;

    /// Import cannot remove the serialized condition protecting nested runtime mount state.
    #[test]
    fn mount_descendant_safety_survives_serialization_and_import() {
        let graph = NodeGraph::new();
        for action in [
            MountAction::Mount {
                source: MountSource::Tmpfs,
                target: "/data".into(),
                options: Vec::new(),
                context: MountContext::Host,
            },
            MountAction::Unmount {
                target: "/data".into(),
                context: MountContext::Host,
            },
        ] {
            let operation = ProviderAction::Mount(action);
            let context = ActionDescriptionContext { graph: &graph };
            let contract = describe_action(&operation, context);
            let json = serde_json::to_string(&contract)
                .unwrap_or_else(|error| panic!("serialize mount safety contract: {error}"));
            let mut restored = serde_json::from_str::<ActionContract>(&json)
                .unwrap_or_else(|error| panic!("deserialize mount safety contract: {error}"));
            assert_eq!(restored, contract);
            assert!(restored.prerequisites.iter().any(|condition| matches!(
                condition,
                ActionPrerequisite::MountDescendantsAbsent { .. }
            )));
            restored.prerequisites.retain(|condition| {
                !matches!(condition, ActionPrerequisite::MountDescendantsAbsent { .. })
            });
            assert!(verify_imported_contract(&operation, restored, context).is_err());
        }
    }

    /// Shrink safety conditions and clean-check postconditions survive JSON and cannot be removed.
    #[test]
    fn filesystem_shrink_contract_preserves_preparation_requirements() {
        let filesystem = NodeId::new();
        let mut graph = NodeGraph::new();
        graph.insert_node(
            filesystem,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::Filesystem {
                        kind: FilesystemKind::Ext4,
                        label: None,
                    },
                    size: Some(Bytes::new(100)),
                },
                size: NodeFacts::default(),
            },
        );
        let context = ActionDescriptionContext { graph: &graph };
        let shrink = ProviderAction::Filesystem(FilesystemAction::Resize {
            filesystem,
            new_size: Bytes::new(60),
        });
        let contract = describe_action(&shrink, context);
        for prerequisite in [
            ActionPrerequisite::FilesystemUnmounted(filesystem),
            ActionPrerequisite::FilesystemCheckPassed(filesystem),
            ActionPrerequisite::FilesystemMinimumSize {
                filesystem,
                target: Bytes::new(60),
            },
        ] {
            assert!(contract.prerequisites.contains(&prerequisite));
            let mut weakened = contract.clone();
            weakened
                .prerequisites
                .retain(|candidate| candidate != &prerequisite);
            assert!(verify_imported_contract(&shrink, weakened, context).is_err());
        }
        let check = describe_action(
            &ProviderAction::Filesystem(FilesystemAction::Check {
                filesystem,
                force: true,
            }),
            context,
        );
        assert!(
            check
                .provided_outputs
                .contains(&ActionOutput::FilesystemCheckPassed(filesystem))
        );
        assert!(
            check
                .postconditions
                .contains(&Postcondition::FilesystemCheckPassed(filesystem))
        );
        for value in [contract, check] {
            let json = serde_json::to_string(&value)
                .unwrap_or_else(|error| panic!("serialize shrink preparation contract: {error}"));
            let restored = serde_json::from_str::<ActionContract>(&json)
                .unwrap_or_else(|error| panic!("deserialize shrink preparation contract: {error}"));
            assert_eq!(restored, value);
        }
    }

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

    /// LUKS open exposes its requested access mode to dependent actions.
    #[test]
    fn luks_open_contract_reports_read_only_interface() {
        let container = NodeId::new();
        let mapping = NodeId::new();
        let mut graph = NodeGraph::new();
        graph.insert_node(
            container,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::LuksContainer {
                        version: LuksVersion::Luks2,
                    },
                    size: Some(Bytes::new(1_000_000)),
                },
                size: NodeFacts::default(),
            },
        );
        let action = ProviderAction::Luks(LuksAction::Open {
            container,
            planned_node_id: mapping,
            mapping_name: "secure".to_owned(),
            credential: crate::CredentialRef::from_uuid(uuid::Uuid::nil()),
            read_only: true,
        });

        let contract = describe_action(&action, ActionDescriptionContext { graph: &graph });

        assert!(
            contract
                .provided_outputs
                .contains(&ActionOutput::BlockInterface {
                    node: mapping,
                    access: BlockInterfaceAccess::ReadOnly,
                })
        );
    }

    /// Activating an LV provides a writable interface without changing its spec.
    #[test]
    fn lvm_activation_contract_reports_writable_interface() {
        let lv = NodeId::new();
        let mut graph = NodeGraph::new();
        graph.insert_node(
            lv,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::LvmLv(LvmLv {
                        name: "data".to_owned(),
                        kind: LvmLvKind::Linear,
                    }),
                    size: Some(Bytes::new(1_000_000)),
                },
                size: NodeFacts::default(),
            },
        );
        let action = ProviderAction::Lvm(LvmAction::SetActivation {
            target: lv,
            active: true,
        });

        let contract = describe_action(&action, ActionDescriptionContext { graph: &graph });

        assert!(
            contract
                .provided_outputs
                .contains(&ActionOutput::BlockInterface {
                    node: lv,
                    access: BlockInterfaceAccess::ReadWrite,
                })
        );
    }

    /// LV creation reserves capacity in the same VG domain it validates.
    #[test]
    fn lvm_create_contract_reserves_volume_group_capacity() {
        let vg = NodeId::new();
        let lv = NodeId::new();
        let size = Bytes::new(60);
        let mut graph = NodeGraph::new();
        graph.insert_node(
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
        );
        let action = ProviderAction::Lvm(LvmAction::CreateLv {
            vg,
            planned_node_id: lv,
            name: "data".to_owned(),
            size,
            layout: LvmLvKind::Linear,
        });
        let domain = AllocationDomain::LvmVolumeGroup(vg);
        let allocation = AllocationQuantity::new(15);

        let contract = describe_action(&action, ActionDescriptionContext { graph: &graph });

        assert!(
            contract
                .prerequisites
                .contains(&ActionPrerequisite::CapacityAvailable {
                    domain: domain.clone(),
                    amount: Some(allocation),
                })
        );
        assert!(
            contract
                .predicted_effect
                .allocations
                .contains(&AllocationEffect::Reserve {
                    domain,
                    amount: allocation,
                })
        );
    }

    /// Existing-LV mutations lock their owning VG rather than using the LV as a metadata domain.
    #[test]
    fn lvm_mutations_use_volume_group_metadata_domain() {
        let vg = NodeId::new();
        let lv = NodeId::new();
        let mut graph = NodeGraph::new();
        graph.insert_node(
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
        );
        graph.insert_node(
            lv,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::LvmLv(LvmLv {
                        name: "data".to_owned(),
                        kind: LvmLvKind::Linear,
                    }),
                    size: Some(Bytes::new(60)),
                },
                size: NodeFacts::default(),
            },
        );
        graph.insert_dependency(Dependency {
            from: vg,
            to: lv,
            kind: DependencyKind::Provides,
        });

        for (action, expected) in [
            (
                LvmAction::ResizeLv {
                    lv,
                    new_size: Bytes::new(80),
                },
                vec![AllocationEffect::Reserve {
                    domain: AllocationDomain::LvmVolumeGroup(vg),
                    amount: AllocationQuantity::new(5),
                }],
            ),
            (
                LvmAction::ResizeLv {
                    lv,
                    new_size: Bytes::new(40),
                },
                vec![AllocationEffect::Release {
                    domain: AllocationDomain::LvmVolumeGroup(vg),
                    amount: AllocationQuantity::new(5),
                }],
            ),
            (
                LvmAction::ResizeLv {
                    lv,
                    new_size: Bytes::new(60),
                },
                Vec::new(),
            ),
            (
                LvmAction::RemoveLv { lv },
                vec![AllocationEffect::Release {
                    domain: AllocationDomain::LvmVolumeGroup(vg),
                    amount: AllocationQuantity::new(15),
                }],
            ),
        ] {
            let contract = describe_action(
                &ProviderAction::Lvm(action),
                ActionDescriptionContext { graph: &graph },
            );

            assert!(contract.affected_resources.contains(&ResourceAccess {
                resource: ActionResource::LvmMetadata(vg),
                mode: ResourceAccessMode::Write,
            }));
            assert!(
                !contract
                    .affected_resources
                    .iter()
                    .any(|access| { access.resource == ActionResource::LvmMetadata(lv) })
            );
            assert_eq!(contract.predicted_effect.allocations, expected);
            let json = serde_json::to_string(&contract)
                .unwrap_or_else(|error| panic!("serialize LVM allocation contract: {error}"));
            let restored = serde_json::from_str::<ActionContract>(&json)
                .unwrap_or_else(|error| panic!("deserialize LVM allocation contract: {error}"));
            assert_eq!(restored, contract);
        }
        let mut node = graph
            .node(&lv)
            .cloned()
            .unwrap_or_else(|| panic!("LV is absent"));
        node.kind.size = None;
        graph.insert_node(lv, node);
        let contract = describe_action(
            &ProviderAction::Lvm(LvmAction::RemoveLv { lv }),
            ActionDescriptionContext { graph: &graph },
        );
        assert_eq!(
            contract.predicted_effect.allocations,
            vec![AllocationEffect::Invalidate {
                domain: AllocationDomain::LvmVolumeGroup(vg)
            }]
        );
        let json = serde_json::to_string(&contract)
            .unwrap_or_else(|error| panic!("serialize unknown LVM allocation: {error}"));
        let restored = serde_json::from_str::<ActionContract>(&json)
            .unwrap_or_else(|error| panic!("deserialize unknown LVM allocation: {error}"));
        assert_eq!(restored, contract);
    }
}
