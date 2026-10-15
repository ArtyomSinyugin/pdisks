//! Versioned historical plan documents; importing never supplies live write permission.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use storage_core::model::{CurrentState, DesiredState};
use storage_provider::{ACTION_SEMANTICS_VERSION, canonicalize_contract};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    ActionDependency, CapacityEvidence, CompositionError, ExecutionPlan, GoalAspect, GoalStatus,
    PlanSimulationError, PlannedAction, ProtectedScope, ProtectionViolation, SimulationReport,
    simulate_plan_for_goal_with_policy,
};

/// Supported plan document schema, independent of provider semantics and transport.
pub const PLAN_SCHEMA_VERSION: u32 = 2;

/// Saved observations and provider evidence used only for historical analysis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalBaseline {
    /// Identity of this saved observation context, not an authentication credential.
    pub id: Uuid,
    /// Immutable observed snapshot against which the plan was analysed.
    pub snapshot: CurrentState,
    /// Capacity observations belonging to this snapshot.
    pub capacity_evidence: Vec<CapacityEvidence>,
}

/// Serializable, untrusted representation of a frozen plan and its review context.
/// Public fields and deserialization do not establish validity; analysis recalculates contracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDocument {
    /// Version of the outer document shape.
    pub schema_version: u32,
    /// Version of the shared provider action-contract catalogue.
    pub action_semantics_version: u32,
    /// Identity of the immutable plan description, separate from any future run.
    pub plan_id: Uuid,
    /// Historical observations retained for repeatable offline analysis.
    pub baseline: HistoricalBaseline,
    /// Original user goal, including ordered explicit intents.
    pub requested_goal: DesiredState,
    /// Explicit preservation policy used during review.
    pub protected_scope: ProtectedScope,
    /// Actions indexed by their stable global identity.
    pub actions: Vec<PlannedAction>,
    /// Required ordering; array order itself has no execution meaning.
    pub dependencies: Vec<ActionDependency>,
}

/// Recalculated historical result; it is never a live admission or execution result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalAnalysis {
    /// Identity retained from the saved plan document.
    pub plan_id: Uuid,
    /// Identity of the historical observations used for this analysis.
    pub baseline_id: Uuid,
    /// Structurally validated plan containing only trusted recalculated contracts.
    pub plan: ExecutionPlan,
    /// Conditional goal analysis, including unresolved live obligations.
    pub report: SimulationReport,
}

/// Failure to encode or validate a historical plan document.
#[derive(Debug, Error)]
pub enum PlanDocumentError {
    /// JSON syntax, typed shape, or non-serializable model data.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The document schema is not supported by this release.
    #[error("unsupported plan schema version {0}")]
    SchemaVersion(u32),
    /// The operation catalogue has different semantics.
    #[error("unsupported action semantics version {0}")]
    SemanticsVersion(u32),
    /// A document or historical observation identity is nil.
    #[error("plan and baseline identities must be non-nil")]
    InvalidIdentity,
    /// The independently supplied historical context does not match the saved one.
    #[error("document baseline differs from the supplied historical baseline")]
    BaselineMismatch,
    /// Invalid identities, references, duplicates, or inconsistent fragment provenance.
    #[error("invalid plan structure: {0}")]
    Structure(String),
    /// Trusted replay rejects a contract or intermediate transition.
    #[error(transparent)]
    Simulation(#[from] PlanSimulationError),
    /// The dependency graph does not coordinate shared resources safely.
    #[error(transparent)]
    Composition(#[from] CompositionError),
    /// The projection contradicts a requested final aspect.
    #[error("plan does not reach goal aspect {0:?}")]
    GoalViolation(GoalAspect),
    /// No supported typed operation covers an explicit goal intent.
    #[error("plan does not cover intent {0}")]
    UncoveredIntent(usize),
    /// One action violates the saved preservation policy.
    #[error("plan modifies protected aspect: {0:?}")]
    ProtectionViolation(ProtectionViolation),
}

impl PlanDocument {
    /// Captures a validated plan with a fresh document and historical-baseline identity.
    /// Unknown evidence permits offline review but never enables real writes.
    pub fn capture(
        plan: &ExecutionPlan,
        current: &CurrentState,
        goal: &DesiredState,
        capacity_evidence: &[CapacityEvidence],
        protected_scope: &ProtectedScope,
    ) -> Result<Self, PlanDocumentError> {
        let document = Self {
            schema_version: PLAN_SCHEMA_VERSION,
            action_semantics_version: ACTION_SEMANTICS_VERSION,
            plan_id: Uuid::now_v7(),
            baseline: HistoricalBaseline {
                id: Uuid::now_v7(),
                snapshot: current.clone(),
                capacity_evidence: capacity_evidence.to_vec(),
            },
            requested_goal: goal.clone(),
            protected_scope: protected_scope.clone(),
            actions: plan.actions().to_vec(),
            dependencies: plan.dependencies().to_vec(),
        };
        document.analyse_historical()?;
        Ok(document)
    }

    /// Parses a document without trusting its versions, baseline, contracts, or scope.
    pub fn from_json(json: &str) -> Result<Self, PlanDocumentError> {
        let value: Value = serde_json::from_str(json)?;
        if let Some(version) = value.get("schema_version").and_then(Value::as_u64)
            && version != u64::from(PLAN_SCHEMA_VERSION)
        {
            return Err(PlanDocumentError::SchemaVersion(
                u32::try_from(version).unwrap_or(u32::MAX),
            ));
        }
        Ok(serde_json::from_str(json)?)
    }

    /// Produces stable JSON for the same description; sequential effects and intents retain order.
    pub fn canonical_json(&self) -> Result<String, PlanDocumentError> {
        self.analyse_historical()?;
        let mut document = self.clone();
        document.actions.sort_by_key(|action| action.id);
        for action in &mut document.actions {
            action.informational_contract =
                canonicalize_contract(action.informational_contract.clone())?;
        }
        let mut value = serde_json::to_value(document)?;
        sort_object_keys(&mut value);
        canonical_graph(&mut value["baseline"]["snapshot"]["graph"]);
        canonical_graph(&mut value["requested_goal"]["graph"]);
        sort_json_array(&mut value["dependencies"], true);
        sort_json_array(&mut value["baseline"]["capacity_evidence"], false);
        sort_json_array(
            &mut value["baseline"]["snapshot"]["mounts"]["entries"],
            false,
        );
        sort_json_array(&mut value["requested_goal"]["mounts"]["runtime"], false);
        sort_json_array(&mut value["requested_goal"]["mounts"]["persistent"], false);
        for field in ["nodes", "runtime_mounts", "persistent_mounts"] {
            sort_json_array(&mut value["protected_scope"][field], true);
        }
        Ok(serde_json::to_string(&value)?)
    }

    /// Reanalyses saved observations as historical data, never as current system evidence.
    pub fn analyse_historical(&self) -> Result<HistoricalAnalysis, PlanDocumentError> {
        if self.schema_version != PLAN_SCHEMA_VERSION {
            return Err(PlanDocumentError::SchemaVersion(self.schema_version));
        }
        if self.action_semantics_version != ACTION_SEMANTICS_VERSION {
            return Err(PlanDocumentError::SemanticsVersion(
                self.action_semantics_version,
            ));
        }
        if self.plan_id.is_nil() || self.baseline.id.is_nil() {
            return Err(PlanDocumentError::InvalidIdentity);
        }
        let draft = self.structural_plan()?;
        let (plan, _) = crate::simulate::replay_plan(
            &self.baseline.snapshot,
            &draft,
            &self.baseline.capacity_evidence,
        )?;
        let contracts = plan
            .actions()
            .iter()
            .map(|action| (action.id, action.informational_contract.clone()))
            .collect();
        crate::compose::validate_resource_order(&contracts, plan.dependencies())?;
        let report = simulate_plan_for_goal_with_policy(
            &self.baseline.snapshot,
            &plan,
            &self.requested_goal,
            &self.baseline.capacity_evidence,
            &self.protected_scope,
        )?;
        if let Some(violation) = report.protection_violations.first() {
            return Err(PlanDocumentError::ProtectionViolation(violation.clone()));
        }
        if let Some(check) = report
            .goal_checks
            .iter()
            .find(|check| matches!(check.status, GoalStatus::Violated { .. }))
        {
            return Err(PlanDocumentError::GoalViolation(check.aspect.clone()));
        }
        if let Some(intent) = report.intent_coverage.iter().find(|intent| !intent.covered) {
            return Err(PlanDocumentError::UncoveredIntent(intent.index));
        }
        Ok(HistoricalAnalysis {
            plan_id: self.plan_id,
            baseline_id: self.baseline.id,
            plan,
            report,
        })
    }

    /// Verifies the saved context against an independently retained historical baseline.
    /// This conservative equality check is not the future live identity/correlation gate.
    pub fn analyse_against(
        &self,
        expected: &HistoricalBaseline,
    ) -> Result<HistoricalAnalysis, PlanDocumentError> {
        let evidence_matches = self.baseline.capacity_evidence.len()
            == expected.capacity_evidence.len()
            && self
                .baseline
                .capacity_evidence
                .iter()
                .all(|item| expected.capacity_evidence.contains(item))
            && expected
                .capacity_evidence
                .iter()
                .all(|item| self.baseline.capacity_evidence.contains(item));
        if self.baseline.id != expected.id
            || canonical_snapshot(&self.baseline.snapshot)?
                != canonical_snapshot(&expected.snapshot)?
            || !evidence_matches
        {
            return Err(PlanDocumentError::BaselineMismatch);
        }
        self.analyse_historical()
    }

    /// Validates indices and references before any array-indexing DAG algorithm runs.
    fn structural_plan(&self) -> Result<ExecutionPlan, PlanDocumentError> {
        for graph in [&self.baseline.snapshot.graph, &self.requested_goal.graph] {
            graph
                .validate_structure()
                .map_err(|error| PlanDocumentError::Structure(error.to_string()))?;
        }
        let mut actions = self.actions.clone();
        actions.sort_by_key(|action| action.id);
        let mut local_ids = HashSet::new();
        let mut origins = HashMap::new();
        for (index, action) in actions.iter().enumerate() {
            if action.id.get() as usize != index {
                return Err(PlanDocumentError::Structure(
                    "action IDs must be unique and contiguous from zero".to_owned(),
                ));
            }
            if !local_ids.insert((action.fragment, action.local_id)) {
                return Err(PlanDocumentError::Structure(
                    "duplicate fragment-local action identity".to_owned(),
                ));
            }
            if origins
                .insert(action.fragment, &action.origin)
                .is_some_and(|origin| origin != &action.origin)
            {
                return Err(PlanDocumentError::Structure(
                    "inconsistent origin within one fragment".to_owned(),
                ));
            }
        }
        let mut edges = HashSet::new();
        for edge in &self.dependencies {
            if edge.before.get() as usize >= actions.len()
                || edge.after.get() as usize >= actions.len()
                || edge.before == edge.after
                || !edges.insert((edge.before, edge.after))
            {
                return Err(PlanDocumentError::Structure(
                    "invalid or duplicate dependency endpoints".to_owned(),
                ));
            }
        }
        crate::compose::topological_order(actions.len(), &self.dependencies)?;
        Ok(ExecutionPlan::new(actions, self.dependencies.clone()))
    }
}

/// Compares historical observations with explicit set-like mount/graph ordering normalized.
fn canonical_snapshot(snapshot: &CurrentState) -> Result<Value, serde_json::Error> {
    let mut value = serde_json::to_value(snapshot)?;
    sort_object_keys(&mut value);
    canonical_graph(&mut value["graph"]);
    sort_json_array(&mut value["mounts"]["entries"], false);
    Ok(value)
}

/// Sorts object members recursively while preserving all array sequencing by default.
fn sort_object_keys(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.sort_keys();
            for child in object.values_mut() {
                sort_object_keys(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                sort_object_keys(child);
            }
        }
        _ => {}
    }
}

/// Orders graph edges, whose model representation is a hash set.
fn canonical_graph(graph: &mut Value) {
    sort_json_array(&mut graph["dependencies"], true);
    sort_json_array(&mut graph["relations"], true);
}

/// Orders only explicitly set-like wire arrays; multiplicity is retained where meaningful.
fn sort_json_array(value: &mut Value, deduplicate: bool) {
    if let Value::Array(array) = value {
        array.sort_by_cached_key(Value::to_string);
        if deduplicate {
            array.dedup();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use storage_core::model::{
        BootSpec, Bytes, Dependency, DependencyKind, Environment, FilesystemKind, LvmLv, LvmLvKind,
        MemberRole, MountContext, MountEntry, MountSource, MountSpec, MountState, Node, NodeFacts,
        NodeGraph, NodeId, NodeKind, NodeSpec, SystemEnvironment,
    };
    use storage_provider::{
        ActionPrerequisite, AllocationDomain, AllocationQuantity, DependencyReason,
        FilesystemAction, FragmentAction, FragmentDependency, FragmentOrigin, LocalActionId,
        LvmAction, MountAction, PlanFragment, ProviderAction, RestartPolicy,
    };

    use crate::{
        ActionId, ProtectedMount, SimulationObligation, compose_plan_for_goal_with_policy,
    };

    use super::*;

    /// Builds a cross-provider document whose later actions consume newly created objects.
    pub(crate) fn document_fixture() -> PlanDocument {
        let disk = NodeId::new();
        let pv = NodeId::new();
        let vg = NodeId::new();
        let lv = NodeId::new();
        let filesystem = NodeId::new();
        let mut graph = NodeGraph::new();
        for (id, kind) in [
            (disk, NodeKind::Disk),
            (pv, NodeKind::LvmPv),
            (
                vg,
                NodeKind::LvmVg {
                    name: "vg".to_owned(),
                    extent_size: Some(Bytes::new(1)),
                },
            ),
        ] {
            graph
                .insert_new(
                    id,
                    Node {
                        kind: NodeSpec { kind, size: None },
                        size: NodeFacts::default(),
                    },
                )
                .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        }
        graph.insert_dependency(Dependency {
            from: disk,
            to: pv,
            kind: DependencyKind::Backs,
        });
        graph.insert_dependency(Dependency {
            from: pv,
            to: vg,
            kind: DependencyKind::MemberOf(MemberRole::LvmPv),
        });
        let current = CurrentState {
            graph,
            mounts: MountState {
                entries: ["/z-last", "/a-first"]
                    .into_iter()
                    .map(|target| storage_core::model::ObservedMount {
                        source: MountSource::Tmpfs,
                        target: PathBuf::from(target),
                        options: Vec::new(),
                        context: MountContext::Host,
                    })
                    .collect(),
            },
            environment: Environment {
                host: SystemEnvironment::default(),
                target: None,
            },
            diagnostics: Vec::new(),
        };
        let mut goal_graph = current.graph.clone();
        goal_graph
            .insert_new(
                lv,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::LvmLv(LvmLv {
                            name: "data".to_owned(),
                            kind: LvmLvKind::Linear,
                        }),
                        size: Some(Bytes::new(32)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        goal_graph
            .insert_new(
                filesystem,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Filesystem {
                            kind: FilesystemKind::Ext4,
                            label: Some("data".to_owned()),
                        },
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        goal_graph.insert_dependency(Dependency {
            from: vg,
            to: lv,
            kind: DependencyKind::Provides,
        });
        goal_graph.insert_dependency(Dependency {
            from: lv,
            to: filesystem,
            kind: DependencyKind::Backs,
        });
        let entry = MountEntry {
            source: MountSource::Filesystem(filesystem),
            target: PathBuf::from("/data"),
            options: vec!["rw".to_owned()],
            persistent: false,
            context: MountContext::Host,
        };
        let goal = DesiredState {
            graph: goal_graph,
            mounts: MountSpec {
                runtime: vec![entry.clone()],
                persistent: Vec::new(),
            },
            boot: BootSpec::Unmanaged,
            intents: Vec::new(),
        };
        let operations = vec![
            ProviderAction::Lvm(LvmAction::CreateLv {
                vg,
                planned_node_id: lv,
                name: "data".to_owned(),
                size: Bytes::new(32),
                layout: LvmLvKind::Linear,
            }),
            ProviderAction::Lvm(LvmAction::SetActivation {
                target: lv,
                active: true,
            }),
            ProviderAction::Filesystem(FilesystemAction::Create {
                backing: lv,
                planned_node_id: filesystem,
                filesystem: FilesystemKind::Ext4,
                label: Some("data".to_owned()),
            }),
            ProviderAction::Mount(MountAction::Mount {
                source: entry.source,
                target: entry.target,
                options: entry.options,
                context: entry.context,
            }),
        ];
        let fragment = PlanFragment {
            actions: operations
                .into_iter()
                .zip(0_u32..)
                .map(|(operation, id)| FragmentAction {
                    id: LocalActionId::new(id),
                    operation,
                })
                .collect(),
            dependencies: (0..3)
                .map(|id| FragmentDependency {
                    before: LocalActionId::new(id),
                    after: LocalActionId::new(id + 1),
                    reason: DependencyReason::RequestedOrder,
                })
                .collect(),
            external_requirements: Vec::new(),
            origin: FragmentOrigin::Change {
                subject: lv,
                description: "create mounted filesystem".to_owned(),
            },
            diagnostics: Vec::new(),
        };
        let evidence = [CapacityEvidence {
            domain: AllocationDomain::LvmVolumeGroup(vg),
            available: AllocationQuantity::new(64),
        }];
        let protected = ProtectedScope {
            nodes: vec![disk],
            runtime_mounts: vec![ProtectedMount {
                target: PathBuf::from("/system"),
                context: MountContext::Host,
            }],
            persistent_mounts: Vec::new(),
        };
        let plan = compose_plan_for_goal_with_policy(
            &current,
            &goal,
            vec![fragment],
            &evidence,
            &protected,
        )
        .unwrap_or_else(|error| panic!("compose wire fixture: {error}"));
        PlanDocument::capture(&plan, &current, &goal, &evidence, &protected)
            .unwrap_or_else(|error| panic!("capture wire fixture: {error}"))
    }

    /// Old mount schemas are rejected explicitly before typed decoding loses their version.
    #[test]
    fn rejects_legacy_mount_schema_before_decoding_goal() {
        let mut value = serde_json::to_value(document_fixture())
            .unwrap_or_else(|error| panic!("encode fixture: {error}"));
        value["schema_version"] = serde_json::json!(1);
        value["requested_goal"]["mounts"] = serde_json::json!({"entries": []});
        assert!(matches!(
            PlanDocument::from_json(&value.to_string()),
            Err(PlanDocumentError::SchemaVersion(1))
        ));
    }

    /// Round-trip replay uses projected predecessor nodes and retains every historical obligation.
    #[test]
    fn canonical_round_trip_preserves_goal_scope_baseline_and_conditional_trace() {
        let document = document_fixture();
        let before = document.clone();
        let json = document
            .canonical_json()
            .unwrap_or_else(|error| panic!("encode document: {error}"));
        let restored = PlanDocument::from_json(&json)
            .unwrap_or_else(|error| panic!("parse document: {error}"));
        let analysis = restored
            .analyse_against(&document.baseline)
            .unwrap_or_else(|error| panic!("replay document: {error}"));
        assert_eq!(analysis.plan_id, document.plan_id);
        assert_eq!(analysis.baseline_id, document.baseline.id);
        assert_eq!(restored.requested_goal, document.requested_goal);
        assert_eq!(restored.protected_scope, document.protected_scope);
        assert_eq!(
            analysis
                .plan
                .actions()
                .iter()
                .map(|action| &action.operation)
                .collect::<Vec<_>>(),
            document
                .actions
                .iter()
                .map(|action| &action.operation)
                .collect::<Vec<_>>()
        );
        assert_eq!(analysis.report.simulation.steps.len(), 4);
        assert!(!analysis.report.is_complete());
        assert!(
            analysis
                .report
                .unresolved_obligations
                .iter()
                .any(|obligation| matches!(
                    obligation,
                    SimulationObligation::Prerequisite {
                        prerequisite: ActionPrerequisite::DestructiveApproval(_),
                        ..
                    }
                ))
        );
        assert_eq!(
            restored
                .canonical_json()
                .unwrap_or_else(|error| panic!("re-encode document: {error}")),
            json
        );
        assert_eq!(document, before);
    }

    /// Semantic sets normalize across input/hash order; returned contracts are trusted recalculations.
    #[test]
    fn canonicalization_accepts_set_order_but_preserves_effect_sequence() {
        let document = document_fixture();
        let expected = document
            .canonical_json()
            .unwrap_or_else(|error| panic!("encode original: {error}"));
        let mut reordered = document.clone();
        reordered.actions.reverse();
        reordered.dependencies.reverse();
        reordered.baseline.snapshot.mounts.entries.reverse();
        for action in &mut reordered.actions {
            let contract = &mut action.informational_contract;
            contract.prerequisites.reverse();
            contract.provided_outputs.reverse();
            contract.affected_resources.reverse();
            contract.invalidated_facts.reverse();
            contract.postconditions.reverse();
        }
        reordered.baseline.snapshot.graph = serde_json::from_str(
            &serde_json::to_string(&document.baseline.snapshot.graph)
                .unwrap_or_else(|error| panic!("serialize graph: {error}")),
        )
        .unwrap_or_else(|error| panic!("rebuild graph hash order: {error}"));
        let analysis = reordered
            .analyse_historical()
            .unwrap_or_else(|error| panic!("replay unordered sets: {error}"));
        assert_eq!(analysis.plan.actions(), document.actions);
        assert_eq!(
            reordered
                .canonical_json()
                .unwrap_or_else(|error| panic!("encode reordered: {error}")),
            expected
        );
        reordered.actions.sort_by_key(|action| action.id);
        reordered.actions[2]
            .informational_contract
            .predicted_effect
            .graph
            .reverse();
        assert!(
            matches!(reordered.analyse_historical(), Err(PlanDocumentError::Simulation(PlanSimulationError::ContractMismatch(id))) if id == ActionId::new(2))
        );
    }

    /// Malformed identities and DAG references are rejected before indexed traversal.
    #[test]
    fn rejects_invalid_structure_and_cycles() {
        let original = document_fixture();
        for case in 0..7 {
            let mut document = original.clone();
            match case {
                0 => document.actions[1].id = ActionId::new(0),
                1 => document.actions[1].local_id = document.actions[0].local_id,
                2 => document.dependencies[0].after = ActionId::new(u32::MAX),
                3 => document.dependencies[0].after = document.dependencies[0].before,
                4 => document.dependencies.push(document.dependencies[0].clone()),
                5 => document.actions[1].origin = FragmentOrigin::Intent { index: 0 },
                _ => {
                    document
                        .baseline
                        .snapshot
                        .graph
                        .insert_dependency(Dependency {
                            from: NodeId::new(),
                            to: NodeId::new(),
                            kind: DependencyKind::Backs,
                        });
                }
            }
            assert!(matches!(
                document.analyse_historical(),
                Err(PlanDocumentError::Structure(_))
            ));
        }
        let mut cyclic = original;
        cyclic.dependencies.push(ActionDependency {
            before: ActionId::new(3),
            after: ActionId::new(0),
            reason: DependencyReason::RequestedOrder,
        });
        assert!(matches!(
            cyclic.analyse_historical(),
            Err(PlanDocumentError::Composition(
                CompositionError::DependencyCycle
            ))
        ));
    }

    /// JSON cannot weaken trusted preconditions, locks, verification, or restart semantics.
    #[test]
    fn rejects_weakened_contracts_and_removed_resource_order() {
        let original = document_fixture();
        for case in 0..4 {
            let mut document = original.clone();
            let contract = &mut document.actions[2].informational_contract;
            match case {
                0 => contract.prerequisites.retain(|prerequisite| {
                    !matches!(prerequisite, ActionPrerequisite::DestructiveApproval(_))
                }),
                1 => contract.affected_resources.clear(),
                2 => contract.postconditions.clear(),
                _ => contract.restart_policy = RestartPolicy::Idempotent,
            }
            assert!(
                matches!(document.analyse_historical(), Err(PlanDocumentError::Simulation(PlanSimulationError::ContractMismatch(id))) if id == ActionId::new(2))
            );
        }
        let mut unordered = original;
        unordered
            .dependencies
            .retain(|edge| !(edge.before == ActionId::new(1) && edge.after == ActionId::new(2)));
        assert!(matches!(
            unordered.analyse_historical(),
            Err(PlanDocumentError::Composition(
                CompositionError::AmbiguousResourceOrder { .. }
            ))
        ));
    }

    /// Saved baseline identities and evidence are checked separately from historical replay.
    #[test]
    fn rejects_wrong_versions_identities_and_historical_baselines() {
        let original = document_fixture();
        let mut document = original.clone();
        document.schema_version += 1;
        assert!(matches!(
            document.analyse_historical(),
            Err(PlanDocumentError::SchemaVersion(_))
        ));
        document = original.clone();
        document.action_semantics_version += 1;
        assert!(matches!(
            document.analyse_historical(),
            Err(PlanDocumentError::SemanticsVersion(_))
        ));
        document = original.clone();
        document.plan_id = Uuid::nil();
        assert!(matches!(
            document.analyse_historical(),
            Err(PlanDocumentError::InvalidIdentity)
        ));
        for case in 0..3 {
            let mut baseline = original.baseline.clone();
            match case {
                0 => baseline.id = Uuid::now_v7(),
                1 => baseline.capacity_evidence[0].available = AllocationQuantity::new(63),
                _ => baseline.snapshot.graph = NodeGraph::new(),
            }
            assert!(matches!(
                original.analyse_against(&baseline),
                Err(PlanDocumentError::BaselineMismatch)
            ));
        }
        let mut value = serde_json::to_value(&original)
            .unwrap_or_else(|error| panic!("encode JSON shape: {error}"));
        value["unexpected"] = Value::Bool(true);
        assert!(matches!(
            PlanDocument::from_json(&value.to_string()),
            Err(PlanDocumentError::Json(_))
        ));
        assert!(matches!(
            PlanDocument::from_json("{}"),
            Err(PlanDocumentError::Json(_))
        ));
    }

    /// Historical import rejects contradicted goals, uncovered operations, and protected writes.
    #[test]
    fn rejects_goal_intent_and_protection_failures() {
        let original = document_fixture();
        let mut document = original.clone();
        document.requested_goal.graph = document.baseline.snapshot.graph.clone();
        assert!(matches!(
            document.analyse_historical(),
            Err(PlanDocumentError::GoalViolation(GoalAspect::Graph))
        ));
        document = original.clone();
        document
            .requested_goal
            .intents
            .push(storage_core::model::OperationIntent::Check {
                target: NodeId::new(),
                force: true,
            });
        assert!(matches!(
            document.analyse_historical(),
            Err(PlanDocumentError::UncoveredIntent(0))
        ));
        document = original;
        if let ProviderAction::Lvm(LvmAction::CreateLv { vg, .. }) = document.actions[0].operation {
            document.protected_scope.nodes.push(vg);
        }
        assert!(matches!(
            document.analyse_historical(),
            Err(PlanDocumentError::ProtectionViolation(_))
        ));
    }
    /// Storage cycles are rejected independently of the action DAG, including empty plans.
    #[test]
    fn rejects_storage_cycles_in_baseline_and_goal() {
        for goal in [false, true] {
            let mut document = document_fixture();
            document.actions.clear();
            document.dependencies.clear();
            let graph = if goal {
                &mut document.requested_goal.graph
            } else {
                &mut document.baseline.snapshot.graph
            };
            let edge = graph
                .dependencies()
                .next()
                .cloned()
                .unwrap_or_else(|| panic!("fixture edge"));
            graph.insert_dependency(Dependency {
                from: edge.to,
                to: edge.from,
                kind: DependencyKind::Backs,
            });
            assert!(matches!(
                document.analyse_historical(),
                Err(PlanDocumentError::Structure(_))
            ));
            let json = serde_json::to_string(&document)
                .unwrap_or_else(|error| panic!("plan JSON: {error}"));
            assert!(PlanDocument::from_json(&json).is_err());
        }
    }

    /// Plan import must not normalize repeated graph identities or edge arrays away.
    #[test]
    fn plan_import_preserves_duplicate_evidence_until_validation() {
        let document = document_fixture();
        let json =
            serde_json::to_string(&document).unwrap_or_else(|error| panic!("plan JSON: {error}"));
        let (id, node) = document
            .baseline
            .snapshot
            .graph
            .nodes()
            .next()
            .unwrap_or_else(|| panic!("fixture node"));
        let entry = format!(
            "\"{}\":{}",
            id.as_uuid(),
            serde_json::to_string(node).unwrap_or_else(|error| panic!("node JSON: {error}"))
        );
        let duplicate = json.replacen(&entry, &format!("{entry},{entry}"), 1);
        assert_ne!(json, duplicate);
        assert!(PlanDocument::from_json(&duplicate).is_err());
        let mut value =
            serde_json::to_value(document).unwrap_or_else(|error| panic!("plan JSON: {error}"));
        let edges = value["baseline"]["snapshot"]["graph"]["dependencies"]
            .as_array_mut()
            .unwrap_or_else(|| panic!("fixture edges"));
        edges.push(edges[0].clone());
        assert!(PlanDocument::from_json(&value.to_string()).is_err());
    }
}
