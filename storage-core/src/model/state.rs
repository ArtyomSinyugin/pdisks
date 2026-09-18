//! Current and desired storage state shared by probing and planning.

use std::{collections::BTreeSet, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::{BtrfsAllocation, FilesystemKind, NodeGraph, NodeId};

/// Observed storage state assembled by probing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentState {
    /// Canonical graph of observed storage objects.
    pub graph: NodeGraph,
    /// Mounts observed in the inspected environments.
    pub mounts: MountState,
    /// Host and target-system environment facts relevant to storage operations.
    pub environment: Environment,
    /// Problems and incomplete observations found while assembling the snapshot.
    pub diagnostics: Vec<Diagnostic>,
}

/// Requested storage state used as planner input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredState {
    /// Requested canonical storage graph.
    pub graph: NodeGraph,
    /// Requested mount configuration.
    pub mounts: MountSpec,
    /// Requested boot configuration.
    pub boot: BootSpec,
    /// Operations that cannot be inferred from the final graph alone.
    pub intents: Vec<OperationIntent>,
}

/// Runtime mounts observed during probing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountState {
    /// Observed mount entries.
    pub entries: Vec<ObservedMount>,
}

/// A runtime mount observed in a particular mount context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedMount {
    /// Storage object or non-block source providing the mounted data.
    pub source: MountSource,
    /// Mount point as seen inside the selected context.
    pub target: PathBuf,
    /// Normalized mount options reported by the operating system.
    pub options: Vec<String>,
    /// Context in which the mount was observed.
    pub context: MountContext,
}

/// Requested mount configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    /// Requested mount entries.
    pub entries: Vec<MountEntry>,
}

/// A requested mount and its persistence policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountEntry {
    /// Storage object or non-block source to mount.
    pub source: MountSource,
    /// Mount point as seen inside the selected context.
    pub target: PathBuf,
    /// Mount options to apply.
    pub options: Vec<String>,
    /// Whether the mount must be written to persistent system configuration.
    pub persistent: bool,
    /// Context in which the mount must exist.
    pub context: MountContext,
}

/// Source of an observed or requested mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountSource {
    /// Filesystem node from the storage graph.
    Filesystem(NodeId),
    /// Btrfs subvolume node from the storage graph.
    BtrfsSubvolume(NodeId),
    /// ZFS dataset node from the storage graph.
    ZfsDataset(NodeId),
    /// Host or target path used as a bind-mount source.
    Bind(PathBuf),
    /// Kernel-provided temporary filesystem.
    Tmpfs,
    /// Typed network filesystem source.
    Network(NetworkMountSource),
    /// Observed source unsupported by the typed model.
    Other(String),
}

/// Supported network filesystem sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMountSource {
    /// NFS export.
    Nfs {
        /// Server name or address.
        server: String,
        /// Export path on the server.
        export: String,
    },
    /// SMB/CIFS share.
    Cifs {
        /// Server name or address.
        server: String,
        /// Share name on the server.
        share: String,
    },
}

/// Filesystem namespace used to interpret a mount target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountContext {
    /// Live host mount namespace.
    Host,
    /// Target system rooted at the supplied path.
    TargetRoot(PathBuf),
    /// Named mount namespace whose lifetime is managed outside this model.
    Namespace(String),
}

/// Storage-relevant facts about the live host and an optional installed target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    /// Environment in which probing and operations run.
    pub host: SystemEnvironment,
    /// Installed system being prepared, when different from the live host.
    pub target: Option<TargetEnvironment>,
}

/// Storage-relevant operating-system facts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemEnvironment {
    /// Operating-system release identifier.
    pub os_release: Option<String>,
    /// Hardware architecture name.
    pub architecture: Option<String>,
    /// Running or selected kernel release.
    pub kernel_release: Option<String>,
    /// Storage tools detected in this environment.
    pub tools: BTreeSet<String>,
    /// Storage-related kernel modules detected in this environment.
    pub kernel_modules: BTreeSet<String>,
}

/// Installed target-system environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetEnvironment {
    /// Root directory through which the target system is accessed.
    pub root: PathBuf,
    /// Facts read from the target system.
    pub system: SystemEnvironment,
}

/// Machine-readable problem found while assembling a storage snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Stable machine-readable diagnostic code.
    pub code: String,
    /// Effect of the diagnostic on later planning or execution.
    pub severity: DiagnosticSeverity,
    /// Model elements to which the diagnostic applies.
    pub subjects: Vec<DiagnosticSubject>,
    /// Human-readable description of the problem.
    pub message: String,
    /// Optional summary of observations supporting the diagnostic.
    pub evidence: Option<String>,
    /// Optional human-readable remediation hint.
    pub suggested_remedy: Option<String>,
}

/// Severity and handling class of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// The requested operation cannot proceed.
    Error,
    /// The state is usable but deserves attention.
    Warning,
    /// The operation may proceed only after explicit confirmation.
    RequiresConfirmation,
    /// Required evidence was not available during probing.
    MissingInformation,
}

/// Model element referenced by a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSubject {
    /// Storage graph node.
    Node(NodeId),
    /// Mount point.
    Mount(PathBuf),
    /// Named model field.
    Field(String),
    /// Host or target environment.
    Environment,
}

/// Requested boot configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootSpec {
    /// Leave boot configuration outside the scope of this desired state.
    Unmanaged,
    /// Configure a UEFI target.
    Uefi(UefiBootSpec),
    /// Configure a legacy BIOS target.
    Bios(BiosBootSpec),
}

/// UEFI boot requirements and firmware changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UefiBootSpec {
    /// Root filesystem node used by the installed system.
    pub root: NodeId,
    /// EFI System Partition node receiving bootloader files.
    pub esp: NodeId,
    /// Bootloader to install.
    pub bootloader: Bootloader,
    /// Initramfs requirements for the selected root dependency chain.
    pub initramfs: InitramfsSpec,
    /// Whether the installer may update firmware NVRAM entries.
    pub update_nvram: bool,
    /// Whether the removable-media fallback boot path must be installed.
    pub removable_path: bool,
}

/// Legacy BIOS boot requirements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BiosBootSpec {
    /// Root filesystem node used by the installed system.
    pub root: NodeId,
    /// Disks or other provider-supported nodes receiving bootloader code.
    pub install_targets: Vec<NodeId>,
    /// BIOS Boot Partition required by GRUB on GPT, when applicable.
    pub bios_boot_partition: Option<NodeId>,
    /// Bootloader to install.
    pub bootloader: Bootloader,
    /// Initramfs requirements for the selected root dependency chain.
    pub initramfs: InitramfsSpec,
}

/// Bootloader implementation requested for the target system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bootloader {
    /// GNU GRUB bootloader.
    Grub,
    /// Provider-defined bootloader not represented by a dedicated variant.
    Other(String),
}

/// Requested initramfs inputs for the target system.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitramfsSpec {
    /// Kernel release for which the initramfs must be generated.
    pub target_kernel: Option<String>,
    /// Features required by the root storage dependency chain.
    pub features: BTreeSet<String>,
}

/// Explicit operation that cannot be derived from the requested final state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationIntent {
    /// Create a new filesystem even when the graph already requests the same kind.
    Reformat {
        /// Node to format.
        target: NodeId,
        /// Filesystem implementation to create.
        filesystem: FilesystemKind,
    },
    /// Replace a member while preserving the containing storage object.
    ReplaceMember {
        /// Array, volume group, pool, or other member-owning node.
        group: NodeId,
        /// Existing member to remove.
        old: NodeId,
        /// Replacement member to add.
        new: NodeId,
    },
    /// Convert Btrfs data, metadata, and system allocation profiles.
    ConvertBtrfsProfiles {
        /// Btrfs filesystem node to convert.
        filesystem: NodeId,
        /// Requested allocation profiles.
        target: BtrfsAllocation,
    },
    /// Check a storage object without changing its declared final shape.
    Check {
        /// Node to check.
        target: NodeId,
        /// Whether to force a check that a provider would otherwise skip.
        force: bool,
    },
    /// Repair a storage object without changing its declared final shape.
    Repair {
        /// Node to repair.
        target: NodeId,
        /// Whether the provider may use its forceful repair mode.
        force: bool,
    },
    /// Activate or deactivate a storage object without changing its topology.
    SetActivation {
        /// Node whose runtime activation state must change.
        target: NodeId,
        /// Requested activation state.
        active: bool,
    },
}
