//! Provider orchestration and assembly of the canonical current state.

use std::collections::{BTreeMap, HashMap};

use storage_core::model::{
    CurrentState, Diagnostic, DiagnosticSeverity, DiagnosticSubject, Environment, MountState,
    NodeGraph, NodeId, ObservedMount, SystemEnvironment,
};
use storage_provider::ProviderBackend;
use thiserror::Error;

/// Successful read-only contribution from one provider.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderState {
    /// Nodes and relationships owned or observed by the provider.
    pub graph: NodeGraph,
    /// Runtime mounts observed by the provider.
    pub mounts: Vec<ObservedMount>,
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
/// Library and CLI adapters must have distinct responsibilities. The assembler
/// never substitutes one adapter for another.
pub trait StateProvider {
    /// Stable provider integration ID.
    fn id(&self) -> &str;

    /// Exclusive area of current state owned by this adapter.
    fn responsibility(&self) -> &str;

    /// Backend used by this adapter.
    fn backend(&self) -> ProviderBackend;

    /// Reads current state without performing storage mutations.
    fn probe(&self) -> Result<ProviderState, ProviderProbeError>;
}

/// Calls providers and assembles their observations into one current state.
pub fn probe(providers: &[&dyn StateProvider]) -> CurrentState {
    let mut by_responsibility: BTreeMap<&str, Vec<&dyn StateProvider>> = BTreeMap::new();
    for provider in providers {
        by_responsibility
            .entry(provider.responsibility())
            .or_default()
            .push(*provider);
    }

    let mut state = empty_current_state();
    let mut owners = HashMap::<NodeId, String>::new();

    for (responsibility, adapters) in by_responsibility {
        if adapters.len() != 1 {
            state
                .diagnostics
                .push(responsibility_conflict(responsibility, &adapters));
            continue;
        }

        let adapter = adapters[0];
        match adapter.probe() {
            Ok(contribution) => {
                merge_provider_state(&mut state, &mut owners, adapter.id(), contribution)
            }
            Err(error) => state.diagnostics.push(provider_failure(
                adapter.id(),
                adapter.backend(),
                format!("{}: {}", error.code, error.message),
            )),
        }
    }

    state
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
    provider_id: &str,
    contribution: ProviderState,
) {
    for (id, node) in contribution.graph.nodes() {
        match state.graph.node(id) {
            Some(existing) if existing != node => state.diagnostics.push(Diagnostic {
                code: "provider.node_conflict".into(),
                severity: DiagnosticSeverity::Error,
                subjects: vec![DiagnosticSubject::Node(*id)],
                message: format!("providers disagree about node {id:?}"),
                evidence: owners.get(id).map(|owner| {
                    format!("first provider: {owner}; conflicting provider: {provider_id}")
                }),
                suggested_remedy: Some("repeat probing and inspect provider evidence".into()),
            }),
            Some(_) => {}
            None => {
                state.graph.insert_node(*id, node.clone());
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
fn provider_failure(provider_id: &str, backend: ProviderBackend, evidence: String) -> Diagnostic {
    Diagnostic {
        code: "provider.probe_failed".into(),
        severity: DiagnosticSeverity::MissingInformation,
        subjects: vec![],
        message: format!(
            "provider {provider_id} ({backend:?}) could not inspect its storage responsibility"
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
                .map(|provider| format!("{} ({:?})", provider.id(), provider.backend()))
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
        backend: ProviderBackend,
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

        fn backend(&self) -> ProviderBackend {
            self.backend
        }

        fn probe(&self) -> Result<ProviderState, ProviderProbeError> {
            self.calls.set(self.calls.get() + 1);
            self.result.clone()
        }
    }

    /// Creates one canonical disk contribution.
    fn disk_state(id: NodeId) -> ProviderState {
        let mut graph = NodeGraph::new();
        graph.insert_node(
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
        );
        ProviderState {
            graph,
            ..ProviderState::default()
        }
    }

    #[test]
    fn providers_with_distinct_responsibilities_are_called_independently() {
        let id = NodeId::from_uuid(Uuid::from_u128(1));
        let native_calls = Rc::new(Cell::new(0));
        let cli_calls = Rc::new(Cell::new(0));
        let native = FakeProvider {
            id: "block",
            responsibility: "block.topology",
            backend: ProviderBackend::Library,
            calls: Rc::clone(&native_calls),
            result: Ok(disk_state(id)),
        };
        let cli = FakeProvider {
            id: "mounts",
            responsibility: "mounts.runtime",
            backend: ProviderBackend::Cli,
            calls: Rc::clone(&cli_calls),
            result: Ok(ProviderState::default()),
        };

        let current = probe(&[&cli, &native]);

        assert_eq!(native_calls.get(), 1);
        assert_eq!(cli_calls.get(), 1);
        assert!(current.graph.node(&id).is_some());
        assert!(current.diagnostics.is_empty());
    }

    #[test]
    fn duplicate_responsibility_is_rejected_without_substitution() {
        let native_calls = Rc::new(Cell::new(0));
        let cli_calls = Rc::new(Cell::new(0));
        let native = FakeProvider {
            id: "libmount",
            responsibility: "mounts.runtime",
            backend: ProviderBackend::Library,
            calls: Rc::clone(&native_calls),
            result: Ok(ProviderState::default()),
        };
        let cli = FakeProvider {
            id: "findmnt",
            responsibility: "mounts.runtime",
            backend: ProviderBackend::Cli,
            calls: Rc::clone(&cli_calls),
            result: Ok(ProviderState::default()),
        };

        let current = probe(&[&cli, &native]);

        assert_eq!(native_calls.get(), 0);
        assert_eq!(cli_calls.get(), 0);
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
            backend: ProviderBackend::Library,
            calls: Rc::new(Cell::new(0)),
            result: Ok(disk_state(id)),
        };
        let mut conflicting = disk_state(id);
        conflicting.graph.insert_node(
            id,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::Zram,
                    size: None,
                },
                size: NodeFacts::default(),
            },
        );
        let second = FakeProvider {
            id: "zram",
            responsibility: "zram.devices",
            backend: ProviderBackend::Library,
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
            backend: ProviderBackend::Library,
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
