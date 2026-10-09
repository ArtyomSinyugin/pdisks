//! Logical LUKS provider separated from the libcryptsetup C ABI adapter.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use storage_core::model::{
    Bytes, Diagnostic, DiagnosticSeverity, DiagnosticSubject, ExternalId, LuksVersion, NodeGraph,
    NodeId, NodeKind,
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    BackendId, CryptsetupEntry, LogicalProvider, NativeProbeError, ProviderId, RegisteredBackend,
};

/// Opaque identity of credential material held outside serializable plans.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialRef(Uuid);

impl CredentialRef {
    /// Creates a reference from an identity assigned by a credential store.
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Returns the opaque identity without resolving credential material.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

/// Typed operations whose semantics belong to the logical LUKS provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LuksAction {
    /// Writes a new LUKS header to a block-providing node.
    Format {
        /// Existing block node to format.
        target: NodeId,
        /// Stable identity reserved for the planned LUKS container node.
        planned_node_id: NodeId,
        /// On-disk LUKS format version.
        version: LuksVersion,
        /// Credential reference resolved only at execution time.
        credential: CredentialRef,
    },
    /// Opens a LUKS container as a dm-crypt mapping.
    Open {
        /// Existing LUKS container node.
        container: NodeId,
        /// Stable identity reserved for the planned dm-crypt mapping node.
        planned_node_id: NodeId,
        /// Requested device-mapper name.
        mapping_name: String,
        /// Credential reference resolved only at execution time.
        credential: CredentialRef,
        /// Whether the mapping must reject writes.
        read_only: bool,
    },
    /// Closes an existing dm-crypt mapping.
    Close {
        /// Existing dm-crypt mapping node.
        mapping: NodeId,
    },
    /// Changes the capacity exposed by an open dm-crypt mapping.
    Resize {
        /// Existing dm-crypt mapping node.
        mapping: NodeId,
        /// Requested capacity, or `None` to consume the available backing size.
        new_size: Option<Bytes>,
    },
}

/// Semantic LUKS header observation produced from backend-specific data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuksObservation {
    /// Block device containing the header.
    pub device: PathBuf,
    /// On-disk LUKS format version.
    pub version: LuksVersion,
    /// Parsed LUKS identity when the backend returned a valid UUID.
    pub identity: Option<ExternalId>,
    /// Offset of encrypted payload data from the start of the backing device.
    pub payload_offset: Bytes,
    /// Original non-secret backend metadata retained for diagnostics and UI.
    pub header: CryptsetupEntry,
}

/// Narrow system-backend contract required by the logical LUKS provider.
pub trait CryptsetupBackend {
    /// Returns the concrete backend identity.
    fn backend_id(&self) -> &BackendId;

    /// Reads non-secret LUKS headers without opening mappings.
    fn probe_headers(&self, devices: &[PathBuf]) -> Result<Vec<CryptsetupEntry>, NativeProbeError>;
}

impl CryptsetupBackend for RegisteredBackend {
    fn backend_id(&self) -> &BackendId {
        &self.manifest().id
    }

    fn probe_headers(&self, devices: &[PathBuf]) -> Result<Vec<CryptsetupEntry>, NativeProbeError> {
        self.probe_cryptsetup(devices)
    }
}

impl<T: CryptsetupBackend + ?Sized> CryptsetupBackend for &T {
    fn backend_id(&self) -> &BackendId {
        (*self).backend_id()
    }

    fn probe_headers(&self, devices: &[PathBuf]) -> Result<Vec<CryptsetupEntry>, NativeProbeError> {
        (*self).probe_headers(devices)
    }
}

/// Logical LUKS semantics backed by one concrete cryptsetup implementation.
pub struct LuksProvider<B> {
    id: ProviderId,
    backend: B,
}

impl<B> LuksProvider<B> {
    /// Creates the built-in logical LUKS provider around a concrete backend.
    pub fn new(backend: B) -> Self {
        Self {
            id: ProviderId::new("luks").unwrap_or_else(|| unreachable!()),
            backend,
        }
    }

    /// Returns the concrete backend borrowed by this provider.
    pub const fn backend(&self) -> &B {
        &self.backend
    }
}

impl<B: CryptsetupBackend> LuksProvider<B> {
    /// Probes headers independently so one device failure keeps other observations.
    pub fn probe(&self, devices: &[PathBuf]) -> LuksProbeReport {
        let mut report = LuksProbeReport::default();
        for device in devices {
            match self.backend.probe_headers(std::slice::from_ref(device)) {
                Ok(headers) => {
                    for header in headers {
                        match translate_header(header) {
                            Ok(observation) => report.observations.push(observation),
                            Err(error) => report.failures.push(LuksProbeFailure {
                                device: device.clone(),
                                error,
                            }),
                        }
                    }
                }
                Err(error) => report.failures.push(LuksProbeFailure {
                    device: device.clone(),
                    error: error.into(),
                }),
            }
        }
        report
    }
}

impl<B: CryptsetupBackend> LogicalProvider for LuksProvider<B> {
    type Action = LuksAction;

    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn backend_id(&self) -> &BackendId {
        self.backend.backend_id()
    }

    fn validate_action(&self, action: &Self::Action, graph: &NodeGraph) -> Vec<Diagnostic> {
        validate_action(action, graph)
    }
}

/// Failure to obtain or interpret LUKS backend observations.
#[derive(Debug, Error)]
pub enum LuksValidationError {
    /// The concrete cryptsetup backend failed.
    #[error(transparent)]
    Backend(#[from] NativeProbeError),
    /// The backend returned an unsupported LUKS type.
    #[error("unsupported LUKS type {luks_type:?} on {device:?}")]
    UnsupportedType {
        /// Device containing the unsupported header.
        device: PathBuf,
        /// Type string copied from libcryptsetup.
        luks_type: String,
    },
}

/// Partial result of probing a set of LUKS devices.
#[derive(Debug, Default)]
pub struct LuksProbeReport {
    /// Successfully translated LUKS observations.
    pub observations: Vec<LuksObservation>,
    /// Device-specific failures that did not discard successful observations.
    pub failures: Vec<LuksProbeFailure>,
}

/// Failure associated with one requested LUKS device.
#[derive(Debug)]
pub struct LuksProbeFailure {
    /// Device whose probe failed.
    pub device: PathBuf,
    /// Backend or translation failure returned for the device.
    pub error: LuksValidationError,
}

/// Converts one libcryptsetup-shaped record into provider-owned semantics.
fn translate_header(header: CryptsetupEntry) -> Result<LuksObservation, LuksValidationError> {
    let version = match header.luks_type.as_str() {
        "LUKS1" => LuksVersion::Luks1,
        "LUKS2" => LuksVersion::Luks2,
        _ => {
            return Err(LuksValidationError::UnsupportedType {
                device: header.device,
                luks_type: header.luks_type,
            });
        }
    };
    let identity = header
        .uuid
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .map(ExternalId::LuksUuid);
    let payload_offset = Bytes::new(header.data_offset_sectors.saturating_mul(512));
    Ok(LuksObservation {
        device: header.device.clone(),
        version,
        identity,
        payload_offset,
        header,
    })
}

/// Checks one LUKS action against the canonical graph.
fn validate_action(action: &LuksAction, graph: &NodeGraph) -> Vec<Diagnostic> {
    match action {
        LuksAction::Format { target, .. } => validate_kind(
            graph,
            *target,
            "luks.format.unsupported_target",
            "LUKS format requires a block-providing target",
            is_format_target,
        ),
        LuksAction::Open {
            container,
            mapping_name,
            ..
        } => {
            let mut diagnostics = validate_kind(
                graph,
                *container,
                "luks.open.not_container",
                "LUKS open requires a LUKS container",
                |kind| matches!(kind, NodeKind::LuksContainer { .. }),
            );
            if mapping_name.is_empty() || mapping_name.contains(['/', '\0']) {
                diagnostics.push(action_diagnostic(
                    "luks.open.invalid_mapping_name",
                    *container,
                    "device-mapper name must be non-empty and contain neither '/' nor NUL",
                ));
            }
            diagnostics
        }
        LuksAction::Close { mapping } => {
            validate_mapping(graph, *mapping, "luks.close.not_mapping")
        }
        LuksAction::Resize { mapping, new_size } => {
            let mut diagnostics = validate_mapping(graph, *mapping, "luks.resize.not_mapping");
            if new_size.is_some_and(|size| size.as_u64() == 0) {
                diagnostics.push(action_diagnostic(
                    "luks.resize.zero_size",
                    *mapping,
                    "LUKS mapping size must be greater than zero",
                ));
            }
            diagnostics
        }
    }
}

/// Checks that a node exists and has an accepted semantic kind.
fn validate_kind(
    graph: &NodeGraph,
    node: NodeId,
    code: &str,
    message: &str,
    accepted: impl FnOnce(&NodeKind) -> bool,
) -> Vec<Diagnostic> {
    let Some(current) = graph.node(&node) else {
        return vec![action_diagnostic(
            "luks.action.missing_target",
            node,
            "LUKS action target is absent from the graph",
        )];
    };
    if accepted(&current.kind.kind) {
        Vec::new()
    } else {
        vec![action_diagnostic(code, node, message)]
    }
}

/// Checks an operation that requires an open dm-crypt mapping.
fn validate_mapping(graph: &NodeGraph, mapping: NodeId, code: &str) -> Vec<Diagnostic> {
    validate_kind(
        graph,
        mapping,
        code,
        "LUKS operation requires an open dm-crypt mapping",
        |kind| matches!(kind, NodeKind::DmCryptMapping { .. }),
    )
}

/// Returns whether the current model kind can provide block storage to LUKS.
fn is_format_target(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Disk
            | NodeKind::NvmeNamespace(_)
            | NodeKind::Loop { .. }
            | NodeKind::DmMultipath
            | NodeKind::Partition { .. }
            | NodeKind::MdArray(_)
            | NodeKind::LvmLv(_)
            | NodeKind::ZfsVolume { .. }
    )
}

/// Builds a stable machine-readable validation diagnostic.
fn action_diagnostic(code: &str, node: NodeId, message: &str) -> Diagnostic {
    Diagnostic {
        code: code.to_owned(),
        severity: DiagnosticSeverity::Error,
        subjects: vec![DiagnosticSubject::Node(node)],
        message: message.to_owned(),
        evidence: None,
        suggested_remedy: None,
    }
}

#[cfg(test)]
mod tests {
    use storage_core::model::{Node, NodeFacts, NodeSpec, Presence};

    use super::*;

    /// In-memory backend used to exercise provider semantics without C FFI.
    struct FakeBackend {
        id: BackendId,
        headers: Vec<CryptsetupEntry>,
        failed_device: Option<PathBuf>,
    }

    impl CryptsetupBackend for FakeBackend {
        fn backend_id(&self) -> &BackendId {
            &self.id
        }

        fn probe_headers(
            &self,
            devices: &[PathBuf],
        ) -> Result<Vec<CryptsetupEntry>, NativeProbeError> {
            if devices
                .iter()
                .any(|device| self.failed_device.as_ref() == Some(device))
            {
                return Err(NativeProbeError::CallFailed {
                    operation: "fake_probe",
                    code: 1,
                });
            }
            Ok(self
                .headers
                .iter()
                .filter(|header| devices.contains(&header.device))
                .cloned()
                .collect())
        }
    }

    /// Creates a graph containing one node with the requested kind.
    fn graph_with(kind: NodeKind) -> (NodeGraph, NodeId) {
        let id = NodeId::new();
        let mut graph = NodeGraph::new();
        graph.insert_node(
            id,
            Node {
                kind: NodeSpec { kind, size: None },
                size: NodeFacts {
                    observed_in: None,
                    presence: Presence::Present,
                    identities: Vec::new(),
                    block: None,
                    device: None,
                },
            },
        );
        (graph, id)
    }

    /// Ensures actions remain serializable provider semantics rather than backend calls.
    #[test]
    fn luks_action_round_trips_without_secret_material() {
        let credential = CredentialRef::from_uuid(
            Uuid::parse_str("b6545f46-c01f-4b70-92f3-82a921539b6f")
                .unwrap_or_else(|error| panic!("parse credential UUID: {error}")),
        );
        let action = LuksAction::Open {
            container: NodeId::new(),
            planned_node_id: NodeId::new(),
            mapping_name: "crypt-root".to_owned(),
            credential,
            read_only: false,
        };
        let json = serde_json::to_string(&action)
            .unwrap_or_else(|error| panic!("serialize LUKS action: {error}"));
        let restored: LuksAction = serde_json::from_str(&json)
            .unwrap_or_else(|error| panic!("deserialize LUKS action: {error}"));

        assert_eq!(restored, action);
        assert!(json.contains(&credential.as_uuid().to_string()));
        assert!(serde_json::from_str::<CredentialRef>(r#""actual-passphrase""#).is_err());
    }

    /// Ensures logical validation rejects opening a non-LUKS node.
    #[test]
    fn open_requires_luks_container() {
        let backend = FakeBackend {
            id: BackendId::new("cryptsetup-luks").unwrap_or_else(|| unreachable!()),
            headers: Vec::new(),
            failed_device: None,
        };
        let provider = LuksProvider::new(backend);
        let (graph, disk) = graph_with(NodeKind::Disk);
        let diagnostics = provider.validate_action(
            &LuksAction::Open {
                container: disk,
                planned_node_id: NodeId::new(),
                mapping_name: "crypt-root".to_owned(),
                credential: CredentialRef::from_uuid(Uuid::nil()),
                read_only: false,
            },
            &graph,
        );

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "luks.open.not_container");
    }

    /// Ensures C-ABI observations are translated at the logical-provider boundary.
    #[test]
    fn provider_translates_cryptsetup_header() {
        let backend = FakeBackend {
            id: BackendId::new("cryptsetup-luks").unwrap_or_else(|| unreachable!()),
            headers: vec![CryptsetupEntry {
                device: PathBuf::from("/dev/vda1"),
                luks_type: "LUKS2".to_owned(),
                uuid: Some("08f959f7-30d8-44c9-a49e-91638f131eb7".to_owned()),
                cipher: Some("aes".to_owned()),
                cipher_mode: Some("xts-plain64".to_owned()),
                data_offset_sectors: 32,
                sector_size: Some(4096),
                volume_key_size: Some(64),
                metadata_size: Some(16_384),
                keyslots_size: Some(16_744_448),
                keyslots: Vec::new(),
            }],
            failed_device: None,
        };
        let provider = LuksProvider::new(backend);
        let report = provider.probe(&[PathBuf::from("/dev/vda1")]);

        assert!(report.failures.is_empty());
        assert_eq!(report.observations.len(), 1);
        assert_eq!(report.observations[0].version, LuksVersion::Luks2);
        assert_eq!(report.observations[0].payload_offset, Bytes::new(16_384));
        assert!(matches!(
            report.observations[0].identity,
            Some(ExternalId::LuksUuid(_))
        ));
    }

    /// Ensures one failed device does not discard another device's observation.
    #[test]
    fn provider_preserves_partial_probe_results() {
        let good_device = PathBuf::from("/dev/vda1");
        let failed_device = PathBuf::from("/dev/vdb1");
        let backend = FakeBackend {
            id: BackendId::new("cryptsetup-luks").unwrap_or_else(|| unreachable!()),
            headers: vec![CryptsetupEntry {
                device: good_device.clone(),
                luks_type: "LUKS2".to_owned(),
                uuid: None,
                cipher: None,
                cipher_mode: None,
                data_offset_sectors: 32,
                sector_size: None,
                volume_key_size: None,
                metadata_size: None,
                keyslots_size: None,
                keyslots: Vec::new(),
            }],
            failed_device: Some(failed_device.clone()),
        };
        let report = LuksProvider::new(backend).probe(&[good_device, failed_device.clone()]);

        assert_eq!(report.observations.len(), 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].device, failed_device);
    }
}
