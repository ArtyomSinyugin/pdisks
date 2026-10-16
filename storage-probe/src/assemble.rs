//! Provider orchestration and assembly of the canonical current state.

use std::collections::{BTreeMap, HashMap, HashSet};

use storage_core::model::{
    CurrentState, Diagnostic, DiagnosticSeverity, DiagnosticSubject, Environment, MountState,
    NodeGraph, NodeId, ObservedMount, SystemEnvironment,
};
use storage_provider::{LvmVgObservation, ProviderConnectionKind};
use thiserror::Error;

/// Successful read-only contribution from one provider.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderState {
    /// Nodes and relationships owned or observed by the provider.
    pub graph: NodeGraph,
    /// Runtime mounts observed by the provider.
    pub mounts: Vec<ObservedMount>,
    /// VG records bound to nodes in this same contribution; not independent later measurements.
    pub lvm_vgs: Vec<LvmVgObservation>,
    /// Environment facts when this provider owns their discovery.
    pub environment: Option<Environment>,
    /// Provider-specific diagnostics produced during successful probing.
    pub diagnostics: Vec<Diagnostic>,
}

/// Failure returned by a provider adapter.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ProviderProbeError {
    /// Stable provider-specific failure code.
    pub code: String,
    /// Human-readable failure description.
    pub message: String,
}

impl ProviderProbeError {
    /// Creates a provider failure with a stable code and readable message.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Provider-specific adapter capable of a read-only probe.
///
/// The connection describes how DisksGraph reaches this logical provider. Concrete
/// C libraries and executables used behind it remain backend implementation
/// details. The assembler never substitutes one provider for another.
pub trait StateProvider {
    /// Stable provider integration ID.
    fn id(&self) -> &str;

    /// Exclusive area of current state owned by this adapter.
    fn responsibility(&self) -> &str;

    /// DisksGraph attachment mechanism used by this provider.
    fn connection_kind(&self) -> ProviderConnectionKind;

    /// Reads current state without performing storage mutations.
    fn probe(&self) -> Result<ProviderState, ProviderProbeError>;
}

/// Calls providers and assembles their observations into one current state.
pub fn probe(providers: &[&dyn StateProvider]) -> CurrentState {
    probe_with_observations(providers).0
}

/// Calls each adapter once and preserves allocation records alongside the resulting graph.
/// Conflicting node contributions cannot supply trusted capacity to this publication.
pub fn probe_with_observations(
    providers: &[&dyn StateProvider],
) -> (CurrentState, Vec<LvmVgObservation>) {
    let mut by_responsibility: BTreeMap<&str, Vec<&dyn StateProvider>> = BTreeMap::new();
    for provider in providers {
        by_responsibility
            .entry(provider.responsibility())
            .or_default()
            .push(*provider);
    }

    let mut state = empty_current_state();
    let mut owners = HashMap::<NodeId, String>::new();
    let mut lvm_vgs = Vec::new();
    let mut conflicts = HashSet::new();

    for (responsibility, adapters) in by_responsibility {
        if adapters.len() != 1 {
            state
                .diagnostics
                .push(responsibility_conflict(responsibility, &adapters));
            continue;
        }

        let adapter = adapters[0];
        match adapter.probe() {
            Ok(mut contribution) => {
                lvm_vgs.append(&mut contribution.lvm_vgs);
                merge_provider_state(
                    &mut state,
                    &mut owners,
                    &mut conflicts,
                    adapter.id(),
                    contribution,
                )
            }
            Err(error) => state.diagnostics.push(provider_failure(
                adapter.id(),
                adapter.connection_kind(),
                format!("{}: {}", error.code, error.message),
            )),
        }
    }

    lvm_vgs.retain(|observation| !conflicts.contains(&observation.node));
    (state, lvm_vgs)
}

/// Creates an empty state before provider contributions are merged.
fn empty_current_state() -> CurrentState {
    CurrentState {
        graph: NodeGraph::new(),
        mounts: MountState::default(),
        environment: Environment {
            host: SystemEnvironment::default(),
            target: None,
        },
        diagnostics: Vec::new(),
    }
}

/// Merges one successful provider contribution without replacing conflicts.
fn merge_provider_state(
    state: &mut CurrentState,
    owners: &mut HashMap<NodeId, String>,
    conflicts: &mut HashSet<NodeId>,
    provider_id: &str,
    contribution: ProviderState,
) {
    for (id, node) in contribution.graph.nodes() {
        match state.graph.node(id) {
            Some(existing) if existing != node => {
                conflicts.insert(*id);
                state.diagnostics.push(Diagnostic {
                    code: "provider.node_conflict".into(),
                    severity: DiagnosticSeverity::Error,
                    subjects: vec![DiagnosticSubject::Node(*id)],
                    message: format!("providers disagree about node {id:?}"),
                    evidence: owners.get(id).map(|owner| {
                        format!("first provider: {owner}; conflicting provider: {provider_id}")
                    }),
                    suggested_remedy: Some("repeat probing and inspect provider evidence".into()),
                });
            }
            Some(_) => {}
            None => {
                state
                    .graph
                    .insert_new(*id, node.clone())
                    .unwrap_or_else(|error| {
                        unreachable!("node absence checked under exclusive access: {error}")
                    });
                owners.insert(*id, provider_id.to_owned());
            }
        }
    }
    for dependency in contribution.graph.dependencies() {
        state.graph.insert_dependency(dependency.clone());
    }
    for relation in contribution.graph.relations() {
        state.graph.insert_relation(relation.clone());
    }
    for mount in contribution.mounts {
        if !state.mounts.entries.contains(&mount) {
            state.mounts.entries.push(mount);
        }
    }
    if let Some(environment) = contribution.environment {
        if state.environment == empty_current_state().environment {
            state.environment = environment;
        } else if state.environment != environment {
            state.diagnostics.push(Diagnostic {
                code: "provider.environment_conflict".into(),
                severity: DiagnosticSeverity::Warning,
                subjects: vec![DiagnosticSubject::Environment],
                message: format!("provider {provider_id} reported conflicting environment facts"),
                evidence: None,
                suggested_remedy: Some("repeat environment probing".into()),
            });
        }
    }
    state.diagnostics.extend(contribution.diagnostics);
}

/// Converts total provider failure into a current-state diagnostic.
fn provider_failure(
    provider_id: &str,
    connection: ProviderConnectionKind,
    evidence: String,
) -> Diagnostic {
    Diagnostic {
        code: "provider.probe_failed".into(),
        severity: DiagnosticSeverity::MissingInformation,
        subjects: vec![],
        message: format!(
            "provider {provider_id} ({connection:?}) could not inspect its storage responsibility"
        ),
        evidence: Some(evidence),
        suggested_remedy: Some("install or repair the provider backend and repeat probing".into()),
    }
}

/// Reports ambiguous ownership instead of choosing a provider implicitly.
fn responsibility_conflict(responsibility: &str, providers: &[&dyn StateProvider]) -> Diagnostic {
    Diagnostic {
        code: "provider.responsibility_conflict".into(),
        severity: DiagnosticSeverity::Error,
        subjects: vec![],
        message: format!("multiple providers own responsibility {responsibility}"),
        evidence: Some(
            providers
                .iter()
                .map(|provider| format!("{} ({:?})", provider.id(), provider.connection_kind()))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        suggested_remedy: Some("assign each provider a distinct responsibility".into()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{cell::Cell, path::PathBuf, rc::Rc};

    use storage_core::model::{
        MountContext, MountSource, Node, NodeFacts, NodeKind, NodeSpec, Presence,
    };
    use uuid::Uuid;

    use super::*;

    /// Deterministic provider used to verify orchestration without loading host libraries.
    struct FakeProvider {
        id: &'static str,
        responsibility: &'static str,
        connection: ProviderConnectionKind,
        calls: Rc<Cell<u32>>,
        result: Result<ProviderState, ProviderProbeError>,
    }

    impl StateProvider for FakeProvider {
        fn id(&self) -> &str {
            self.id
        }

        fn responsibility(&self) -> &str {
            self.responsibility
        }

        fn connection_kind(&self) -> ProviderConnectionKind {
            self.connection
        }

        fn probe(&self) -> Result<ProviderState, ProviderProbeError> {
            self.calls.set(self.calls.get() + 1);
            self.result.clone()
        }
    }

    /// Creates one canonical disk contribution.
    fn disk_state(id: NodeId) -> ProviderState {
        let mut graph = NodeGraph::new();
        graph
            .insert_new(
                id,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: None,
                    },
                    size: NodeFacts {
                        presence: Presence::Present,
                        ..NodeFacts::default()
                    },
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        ProviderState {
            graph,
            ..ProviderState::default()
        }
    }

    /// Builds a raw VG allocation record for publication-boundary tests.
    fn allocation(node: NodeId) -> LvmVgObservation {
        LvmVgObservation {
            node,
            vg: storage_provider::LvmVgEntry {
                name: "vg0".to_owned(),
                uuid: "vg-uuid".to_owned(),
                size: 400 * 1024 * 1024,
                extent_size: 4 * 1024 * 1024,
                free: 200 * 1024 * 1024,
                pv_count: 1,
                exported: false,
            },
        }
    }

    /// Graph and capacity come from exactly the same adapter invocation and round-trip together.
    #[test]
    fn allocation_records_are_published_without_a_second_probe() {
        let node = NodeId::new();
        let calls = Rc::new(Cell::new(0));
        let mut contribution = disk_state(node);
        contribution.lvm_vgs.push(allocation(node));
        let provider = FakeProvider {
            id: "vg",
            responsibility: "vg",
            connection: ProviderConnectionKind::BuiltIn,
            calls: calls.clone(),
            result: Ok(contribution),
        };
        let (current, lvm_vgs) = probe_with_observations(&[&provider]);
        assert_eq!(calls.get(), 1);
        assert!(current.graph.node(&node).is_some());
        assert_eq!(lvm_vgs, vec![allocation(node)]);
        let snapshot = crate::ProbeSnapshot {
            generation: 1,
            current,
            lvm_vgs,
        };
        let mut value = serde_json::to_value(&snapshot).expect("encode snapshot");
        assert_eq!(
            serde_json::from_value::<crate::ProbeSnapshot>(value.clone()).expect("decode snapshot"),
            snapshot
        );
        value
            .as_object_mut()
            .expect("snapshot object")
            .remove("lvm_vgs");
        assert!(
            serde_json::from_value::<crate::ProbeSnapshot>(value)
                .expect("legacy snapshot")
                .lvm_vgs
                .is_empty()
        );
    }

    /// No capacity survives disagreement about its node, even if only the first adapter supplied it.
    #[test]
    fn conflicting_nodes_and_responsibilities_cannot_publish_capacity() {
        let node = NodeId::new();
        let mut first = disk_state(node);
        first.lvm_vgs.push(allocation(node));
        let mut second = disk_state(node);
        second
            .graph
            .replace_existing(
                node,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: Some(storage_core::model::Bytes::new(1)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .expect("change node");
        let calls = Rc::new(Cell::new(0));
        let first = FakeProvider {
            id: "first",
            responsibility: "one",
            connection: ProviderConnectionKind::BuiltIn,
            calls: calls.clone(),
            result: Ok(first),
        };
        let mut second = FakeProvider {
            id: "second",
            responsibility: "two",
            connection: ProviderConnectionKind::BuiltIn,
            calls: calls.clone(),
            result: Ok(second),
        };
        let (current, observations) = probe_with_observations(&[&first, &second]);
        assert!(observations.is_empty());
        assert!(
            current
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "provider.node_conflict")
        );
        second.responsibility = "one";
        let (_, observations) = probe_with_observations(&[&first, &second]);
        assert!(observations.is_empty());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn providers_with_distinct_responsibilities_are_called_independently() {
        let id = NodeId::from_uuid(Uuid::from_u128(1));
        let built_in_calls = Rc::new(Cell::new(0));
        let process_calls = Rc::new(Cell::new(0));
        let built_in = FakeProvider {
            id: "block",
            responsibility: "block.topology",
            connection: ProviderConnectionKind::BuiltIn,
            calls: Rc::clone(&built_in_calls),
            result: Ok(disk_state(id)),
        };
        let process = FakeProvider {
            id: "mounts",
            responsibility: "mounts.runtime",
            connection: ProviderConnectionKind::Process,
            calls: Rc::clone(&process_calls),
            result: Ok(ProviderState::default()),
        };

        let current = probe(&[&process, &built_in]);

        assert_eq!(built_in_calls.get(), 1);
        assert_eq!(process_calls.get(), 1);
        assert!(current.graph.node(&id).is_some());
        assert!(current.diagnostics.is_empty());
    }

    #[test]
    fn duplicate_responsibility_is_rejected_without_substitution() {
        let built_in_calls = Rc::new(Cell::new(0));
        let process_calls = Rc::new(Cell::new(0));
        let built_in = FakeProvider {
            id: "libmount",
            responsibility: "mounts.runtime",
            connection: ProviderConnectionKind::BuiltIn,
            calls: Rc::clone(&built_in_calls),
            result: Ok(ProviderState::default()),
        };
        let process = FakeProvider {
            id: "findmnt",
            responsibility: "mounts.runtime",
            connection: ProviderConnectionKind::Process,
            calls: Rc::clone(&process_calls),
            result: Ok(ProviderState::default()),
        };

        let current = probe(&[&process, &built_in]);

        assert_eq!(built_in_calls.get(), 0);
        assert_eq!(process_calls.get(), 0);
        assert!(
            current
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "provider.responsibility_conflict")
        );
    }

    #[test]
    fn conflicting_provider_nodes_are_not_silently_replaced() {
        let id = NodeId::from_uuid(Uuid::from_u128(3));
        let first = FakeProvider {
            id: "block",
            responsibility: "block.topology",
            connection: ProviderConnectionKind::BuiltIn,
            calls: Rc::new(Cell::new(0)),
            result: Ok(disk_state(id)),
        };
        let mut conflicting = disk_state(id);
        conflicting
            .graph
            .replace_existing(
                id,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Zram,
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let second = FakeProvider {
            id: "zram",
            responsibility: "zram.devices",
            connection: ProviderConnectionKind::BuiltIn,
            calls: Rc::new(Cell::new(0)),
            result: Ok(conflicting),
        };

        let current = probe(&[&first, &second]);

        assert!(matches!(
            current.graph.node(&id).map(|node| &node.kind.kind),
            Some(NodeKind::Disk)
        ));
        assert!(
            current
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "provider.node_conflict")
        );
    }

    #[test]
    fn provider_contribution_populates_every_current_state_section() {
        let id = NodeId::from_uuid(Uuid::from_u128(4));
        let mut contribution = disk_state(id);
        contribution.mounts.push(ObservedMount {
            source: MountSource::Tmpfs,
            target: PathBuf::from("/run"),
            options: vec!["rw".into()],
            context: MountContext::Host,
        });
        contribution.environment = Some(Environment {
            host: SystemEnvironment {
                os_release: Some("ALT".into()),
                architecture: Some("x86_64".into()),
                kernel_release: Some("test".into()),
                ..SystemEnvironment::default()
            },
            target: None,
        });
        contribution.diagnostics.push(Diagnostic {
            code: "provider.notice".into(),
            severity: DiagnosticSeverity::Warning,
            subjects: vec![DiagnosticSubject::Node(id)],
            message: "provider notice".into(),
            evidence: None,
            suggested_remedy: None,
        });
        let provider = FakeProvider {
            id: "block",
            responsibility: "block.topology",
            connection: ProviderConnectionKind::BuiltIn,
            calls: Rc::new(Cell::new(0)),
            result: Ok(contribution),
        };

        let current = probe(&[&provider]);

        assert!(current.graph.node(&id).is_some());
        assert_eq!(current.mounts.entries.len(), 1);
        assert_eq!(current.environment.host.os_release.as_deref(), Some("ALT"));
        assert_eq!(current.diagnostics[0].code, "provider.notice");
    }
}
