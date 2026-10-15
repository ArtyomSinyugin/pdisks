//! Atomic interpretation of provider effects on DesiredState.

use crate::prerequisites::{evaluate_prerequisites, has_mounted_descendant, mount_at};
use crate::simulation_evidence::{
    ActionSimulationError, CheckStatus, EffectApplicationError, SimulatedAction,
    SimulationEvidence, apply_allocation_effects, apply_outputs, invalidate_filesystem_checks,
    invalidate_observations, mark_persistent_target,
};
use storage_core::model::{
    DesiredState, DiagnosticSeverity, FilesystemKind, MountEntry, MountSource, Node, NodeFacts,
    NodeId, NodeKind,
};
use storage_provider::{
    ActionDescriptionContext, FactInvalidation, GraphEffect, MountEffect, PredictedEffect,
    ProviderAction, describe_action, validate_provider_action,
};

/// Describes, checks, and conditionally projects one typed provider action.
///
/// Unknown checks are retained in the result and permit a conditional
/// projection. A known violation prevents the effect from being applied.
pub(crate) fn simulate_action(
    desired: &mut DesiredState,
    evidence: &mut SimulationEvidence,
    action: &ProviderAction,
) -> Result<SimulatedAction, ActionSimulationError> {
    let diagnostics = validate_provider_action(action, &desired.graph);
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
    {
        return Err(ActionSimulationError::Validation { diagnostics });
    }
    let contract = describe_action(
        action,
        ActionDescriptionContext {
            graph: &desired.graph,
        },
    );
    let checks = evaluate_prerequisites(desired, evidence, &contract.prerequisites);
    if checks
        .iter()
        .any(|check| matches!(check.status, CheckStatus::Violated { .. }))
    {
        return Err(ActionSimulationError::PrerequisitesViolated { checks });
    }
    let invalidated_facts = apply_effect_with_invalidations(
        desired,
        evidence,
        &contract.predicted_effect,
        &contract.invalidated_facts,
    )?;
    apply_outputs(evidence, &contract.provided_outputs);
    Ok(SimulatedAction {
        contract,
        checks,
        invalidated_facts,
    })
}

/// Atomically applies one trusted effect to configuration and evidence for isolated tests.
#[cfg(test)]
pub(crate) fn apply_effect(
    desired: &mut DesiredState,
    evidence: &mut SimulationEvidence,
    effect: &PredictedEffect,
) -> Result<(), EffectApplicationError> {
    apply_effect_with_invalidations(desired, evidence, effect, &[]).map(|_| ())
}

/// Publishes configuration and evidence only after every effect and invariant succeeds.
fn apply_effect_with_invalidations(
    desired: &mut DesiredState,
    evidence: &mut SimulationEvidence,
    effect: &PredictedEffect,
    invalidations: &[FactInvalidation],
) -> Result<Vec<FactInvalidation>, EffectApplicationError> {
    let mut next_desired = desired.clone();
    let mut next_evidence = evidence.clone();
    let invalidated_facts =
        invalidate_observations(&next_desired, &mut next_evidence, invalidations);
    invalidate_filesystem_checks(&next_desired, &mut next_evidence, invalidations);
    apply_graph_effects(&mut next_desired, &mut next_evidence, &effect.graph)?;
    apply_mount_effects(&mut next_desired, &mut next_evidence, &effect.mounts)?;
    apply_allocation_effects(&mut next_evidence, &effect.allocations)?;
    next_desired.graph.validate_structure()?;
    *desired = next_desired;
    *evidence = next_evidence;
    Ok(invalidated_facts)
}

/// Applies graph changes to an unpublished candidate desired state.
fn apply_graph_effects(
    desired: &mut DesiredState,
    evidence: &mut SimulationEvidence,
    effects: &[GraphEffect],
) -> Result<(), EffectApplicationError> {
    for effect in effects {
        match effect {
            GraphEffect::InsertNode { id, spec } => {
                if desired.graph.node(id).is_some() {
                    return Err(EffectApplicationError::DuplicateNode(*id));
                }
                desired.graph.insert_new(
                    *id,
                    Node {
                        kind: spec.clone(),
                        size: NodeFacts::default(),
                    },
                )?;
                evidence.projected_graph_nodes.push(*id);
            }
            GraphEffect::RemoveNode(node) => {
                if desired.graph.remove_node(node).is_none() {
                    return Err(EffectApplicationError::MissingNode(*node));
                }
                evidence
                    .projected_graph_nodes
                    .retain(|candidate| candidate != node);
                evidence.activations.remove(node);
                evidence.block_interfaces.remove(node);
            }
            GraphEffect::SetNodeSize { node, size } => {
                update_node(desired, evidence, *node, "set node size", |value| {
                    value.kind.size = Some(*size);
                    true
                })?;
            }
            GraphEffect::InvalidateNodeSize(node) => {
                update_node(desired, evidence, *node, "invalidate node size", |value| {
                    value.kind.size = None;
                    true
                })?;
            }
            GraphEffect::SetPartitionAttributes {
                partition,
                attributes,
            } => {
                update_node(
                    desired,
                    evidence,
                    *partition,
                    "set partition attributes",
                    |value| {
                        let NodeKind::Partition {
                            attributes: current,
                            ..
                        } = &mut value.kind.kind
                        else {
                            return false;
                        };
                        *current = *attributes;
                        true
                    },
                )?;
            }
            GraphEffect::SetFilesystemLabel { filesystem, label } => {
                update_node(
                    desired,
                    evidence,
                    *filesystem,
                    "set filesystem label",
                    |value| {
                        let NodeKind::Filesystem { label: current, .. } = &mut value.kind.kind
                        else {
                            return false;
                        };
                        *current = label.clone();
                        true
                    },
                )?;
            }
            GraphEffect::SetBtrfsAllocation {
                filesystem,
                allocation,
            } => {
                update_node(
                    desired,
                    evidence,
                    *filesystem,
                    "set Btrfs allocation",
                    |value| {
                        let NodeKind::Filesystem {
                            kind: FilesystemKind::Btrfs(current),
                            ..
                        } = &mut value.kind.kind
                        else {
                            return false;
                        };
                        *current = allocation.clone();
                        true
                    },
                )?;
            }
            GraphEffect::InsertDependency(dependency) => {
                desired.graph.insert_dependency(dependency.clone());
            }
            GraphEffect::RemoveDependency(dependency) => {
                if !desired.graph.remove_dependency(dependency) {
                    return Err(EffectApplicationError::MissingDependency);
                }
            }
            GraphEffect::InsertRelation(relation) => {
                desired.graph.insert_relation(relation.clone());
            }
            GraphEffect::RemoveRelation(relation) => {
                if !desired.graph.remove_relation(relation) {
                    return Err(EffectApplicationError::MissingRelation);
                }
            }
            GraphEffect::SetActivation { node, active } => {
                if desired.graph.node(node).is_none() {
                    return Err(EffectApplicationError::MissingNode(*node));
                }
                evidence.activations.insert(*node, *active);
                if !active {
                    evidence.block_interfaces.remove(node);
                }
            }
        }
    }
    Ok(())
}

/// Applies mount changes without conflating runtime and persistent state.
fn apply_mount_effects(
    desired: &mut DesiredState,
    evidence: &mut SimulationEvidence,
    effects: &[MountEffect],
) -> Result<(), EffectApplicationError> {
    for effect in effects {
        match effect {
            MountEffect::AddRuntime {
                source,
                target,
                options,
                context,
            } => {
                if mount_at(desired, target, context).is_some() {
                    return Err(EffectApplicationError::OccupiedMountTarget(target.clone()));
                }
                if has_mounted_descendant(desired, target, context) {
                    return Err(EffectApplicationError::MountedDescendant(target.clone()));
                }
                if let MountSource::Filesystem(filesystem) = source {
                    evidence.checked_filesystems.remove(filesystem);
                } else if matches!(source, MountSource::Bind(_) | MountSource::Other(_)) {
                    evidence.checked_filesystems.clear();
                }
                evidence.projected_runtime_mounts = true;
                desired.mounts.runtime.push(MountEntry {
                    source: source.clone(),
                    target: target.clone(),
                    options: options.clone(),
                    context: context.clone(),
                });
            }
            MountEffect::RemoveRuntime { target, context } => {
                if has_mounted_descendant(desired, target, context) {
                    return Err(EffectApplicationError::MountedDescendant(target.clone()));
                }
                evidence.projected_runtime_mounts = true;
                desired
                    .mounts
                    .runtime
                    .retain(|mount| mount.target != *target || mount.context != *context);
            }
            MountEffect::SetPersistent(entry) => {
                desired.mounts.persistent.retain(|current| {
                    current.target != entry.target || current.context != entry.context
                });
                desired.mounts.persistent.push(entry.clone());
                mark_persistent_target(evidence, &entry.target, &entry.context);
            }
            MountEffect::RemovePersistent { target, context } => {
                desired
                    .mounts
                    .persistent
                    .retain(|entry| entry.target != *target || entry.context != *context);
                mark_persistent_target(evidence, target, context);
            }
        }
    }
    Ok(())
}

/// Replaces one node only after a type-specific update succeeds.
fn update_node(
    desired: &mut DesiredState,
    evidence: &mut SimulationEvidence,
    id: NodeId,
    effect: &'static str,
    update: impl FnOnce(&mut Node) -> bool,
) -> Result<(), EffectApplicationError> {
    let Some(mut node) = desired.graph.node(&id).cloned() else {
        return Err(EffectApplicationError::MissingNode(id));
    };
    if !update(&mut node) {
        return Err(EffectApplicationError::WrongNodeKind { node: id, effect });
    }
    desired.graph.replace_existing(id, node)?;
    if !evidence.projected_graph_nodes.contains(&id) {
        evidence.projected_graph_nodes.push(id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::simulation_evidence::{CheckEvidence, PrerequisiteCheck, TestSimulation};
    use std::path::PathBuf;
    use storage_core::model::{
        Bytes, Diagnostic, Environment, FilesystemKind, LvmLv, LvmLvKind, MountSource, MountState,
        NodeSpec, SystemEnvironment,
    };
    use storage_core::model::{
        CurrentState, DependencyKind, MountContext, NodeGraph, ObservedMount,
    };
    use storage_provider::{
        ActionContract, ActionDescriptionContext, ActionOutput, ActionPrerequisite,
        AllocationDomain, AllocationQuantity, BlockInterfaceAccess, FactKind, FactScope,
    };
    use storage_provider::{FilesystemAction, LvmAction, ProviderAction};

    use super::*;

    /// Builds a mapping with a historical writable endpoint and one filesystem consumer.
    fn current_with_observed_mapping() -> (CurrentState, NodeId, NodeId) {
        let (mut current, disk) = current_with_disk();
        let mapping = NodeId::new();
        current
            .graph
            .insert_new(
                mapping,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::DmCryptMapping {
                            name: "data".to_owned(),
                        },
                        size: Some(Bytes::new(100)),
                    },
                    size: NodeFacts {
                        block: Some(storage_core::model::BlockFacts {
                            paths: vec![PathBuf::from("/dev/mapper/data")],
                            devno: storage_core::model::DeviceNumber {
                                major: 253,
                                minor: 0,
                            },
                            geometry: None,
                            read_only: Some(false),
                        }),
                        ..NodeFacts::default()
                    },
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        current
            .graph
            .insert_dependency(storage_core::model::Dependency {
                from: disk,
                to: mapping,
                kind: DependencyKind::Backs,
            });
        let filesystem = NodeId::new();
        current
            .graph
            .insert_new(
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
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        current
            .graph
            .insert_dependency(storage_core::model::Dependency {
                from: mapping,
                to: filesystem,
                kind: DependencyKind::Backs,
            });
        (current, mapping, filesystem)
    }

    /// An automatic LUKS resize cannot retain the old observed capacity as future evidence.
    #[test]
    fn automatic_luks_resize_invalidates_the_previous_size() {
        let (current, mapping, filesystem) = current_with_observed_mapping();
        let baseline = current.clone();
        let mut projection = TestSimulation::from_current(&current);
        let result = projection
            .simulate_action(&ProviderAction::Luks(
                storage_provider::LuksAction::Resize {
                    mapping,
                    new_size: None,
                },
            ))
            .unwrap_or_else(|error| panic!("simulate automatic mapping resize: {error}"));
        assert_eq!(
            projection.graph().node(&mapping).map(|node| node.kind.size),
            Some(None)
        );
        assert!(result.invalidated_facts.contains(&FactInvalidation {
            scope: FactScope::Node(mapping),
            kind: FactKind::Geometry
        }));
        assert!(projection.is_observation_stale(&FactScope::Node(mapping), FactKind::Geometry));
        let checks = projection.evaluate_prerequisites(&[ActionPrerequisite::BackingSizeAtLeast {
            node: filesystem,
            size: Bytes::new(150),
        }]);
        assert!(matches!(checks[0].status, CheckStatus::Unknown { .. }));
        assert_eq!(current, baseline);
    }

    /// Geometry predictions preserve access observations without relabelling them as new evidence.
    #[test]
    fn predicted_size_does_not_fabricate_new_access_observations() {
        let (current, mapping, filesystem) = current_with_observed_mapping();
        let historical = current.graph.node(&mapping).map(|node| node.size.clone());
        let mut projection = TestSimulation::from_current(&current);
        projection
            .simulate_action(&ProviderAction::Luks(
                storage_provider::LuksAction::Resize {
                    mapping,
                    new_size: Some(Bytes::new(200)),
                },
            ))
            .unwrap_or_else(|error| panic!("simulate explicit mapping resize: {error}"));
        let checks = projection.evaluate_prerequisites(&[
            ActionPrerequisite::Writable(mapping),
            ActionPrerequisite::BackingSizeAtLeast {
                node: filesystem,
                size: Bytes::new(150),
            },
        ]);
        assert_eq!(
            checks[0].status,
            CheckStatus::Satisfied(CheckEvidence::ObservedSnapshot)
        );
        assert_eq!(
            checks[1].status,
            CheckStatus::Satisfied(CheckEvidence::PredictedEffect)
        );
        assert!(projection.is_observation_stale(&FactScope::Node(mapping), FactKind::Geometry));
        assert!(!projection.is_observation_stale(&FactScope::Node(mapping), FactKind::Activation));
        assert_eq!(
            projection
                .graph()
                .node(&mapping)
                .map(|node| node.size.clone()),
            historical
        );
    }

    /// Group activation invalidates child access claims; explicit re-preparation supplies a new guarantee.
    #[test]
    fn activation_invalidation_revokes_descendant_endpoint_evidence() {
        let (mut current, lv, filesystem) = current_with_observed_mapping();
        let vg = NodeId::new();
        let mut node = current
            .graph
            .node(&lv)
            .cloned()
            .unwrap_or_else(|| panic!("endpoint is absent"));
        node.kind.kind = NodeKind::LvmLv(LvmLv {
            name: "data".to_owned(),
            kind: LvmLvKind::Linear,
        });
        current
            .graph
            .replace_existing(lv, node)
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        current
            .graph
            .insert_new(
                vg,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::LvmVg {
                            name: "vg0".to_owned(),
                            extent_size: Some(Bytes::new(1)),
                        },
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        current
            .graph
            .insert_dependency(storage_core::model::Dependency {
                from: vg,
                to: lv,
                kind: DependencyKind::Provides,
            });
        let baseline = current.clone();
        let mut projection = TestSimulation::from_current(&current);
        let activate_lv = ProviderAction::Lvm(LvmAction::SetActivation {
            target: lv,
            active: true,
        });
        projection
            .simulate_action(&activate_lv)
            .unwrap_or_else(|error| panic!("prepare child endpoint: {error}"));
        let result = projection
            .simulate_action(&ProviderAction::Lvm(LvmAction::SetActivation {
                target: vg,
                active: false,
            }))
            .unwrap_or_else(|error| panic!("invalidate group activation: {error}"));
        assert!(result.invalidated_facts.contains(&FactInvalidation {
            scope: FactScope::Node(lv),
            kind: FactKind::Activation
        }));
        assert_eq!(projection.block_interface(&lv), None);
        let checks = projection.evaluate_prerequisites(&[
            ActionPrerequisite::Writable(lv),
            ActionPrerequisite::MountSourceAvailable(MountSource::Filesystem(filesystem)),
        ]);
        assert!(
            checks
                .iter()
                .all(|check| matches!(check.status, CheckStatus::Unknown { .. }))
        );
        assert_eq!(
            projection
                .graph()
                .node(&lv)
                .and_then(|node| node.size.block.as_ref())
                .and_then(|block| block.read_only),
            Some(false)
        );
        projection
            .simulate_action(&activate_lv)
            .unwrap_or_else(|error| panic!("restore child endpoint guarantee: {error}"));
        let checks = projection.evaluate_prerequisites(&[
            ActionPrerequisite::Writable(lv),
            ActionPrerequisite::MountSourceAvailable(MountSource::Filesystem(filesystem)),
        ]);
        assert_eq!(
            checks[0].status,
            CheckStatus::Satisfied(CheckEvidence::PredictedEffect)
        );
        assert_eq!(
            checks[1].status,
            CheckStatus::Satisfied(CheckEvidence::PredictedEffect)
        );
        assert!(projection.is_observation_stale(&FactScope::Node(lv), FactKind::Activation));
        assert_eq!(current, baseline);
    }

    /// A failed action cannot publish staleness, graph mutations, or revoked runtime guarantees.
    #[test]
    fn failed_action_preserves_observation_validity() {
        let (mut current, _) = current_with_disk();
        let vg = NodeId::new();
        current
            .graph
            .insert_new(
                vg,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::LvmVg {
                            name: "vg0".to_owned(),
                            extent_size: Some(Bytes::new(1)),
                        },
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let mut projection = TestSimulation::from_current(&current);
        let before = projection.clone();
        let result = projection.simulate_action(&ProviderAction::Lvm(LvmAction::CreateLv {
            vg,
            planned_node_id: vg,
            name: "collision".to_owned(),
            size: Bytes::new(100),
            layout: LvmLvKind::Linear,
        }));
        assert!(matches!(
            result,
            Err(ActionSimulationError::Effect(
                EffectApplicationError::DuplicateNode(_)
            ))
        ));
        assert_eq!(projection, before);
        assert!(projection.invalidated_facts().is_empty());
    }

    /// Parent mount changes cannot hide or detach a still-mounted descendant.
    #[test]
    fn parent_mount_changes_reject_active_descendants_atomically() {
        let (mut current, _) = current_with_disk();
        current.mounts.entries.push(ObservedMount {
            source: MountSource::Tmpfs,
            target: PathBuf::from("/data/child"),
            options: Vec::new(),
            context: MountContext::Host,
        });
        for action in [
            storage_provider::MountAction::Mount {
                source: MountSource::Tmpfs,
                target: PathBuf::from("/data"),
                options: Vec::new(),
                context: MountContext::Host,
            },
            storage_provider::MountAction::Unmount {
                target: PathBuf::from("/data"),
                context: MountContext::Host,
            },
        ] {
            let mut projection = TestSimulation::from_current(&current);
            let original = projection.clone();
            let operation = ProviderAction::Mount(action);
            assert!(projection.simulate_action(&operation).is_err());
            assert_eq!(projection, original);
            let effect = describe_action(
                &operation,
                ActionDescriptionContext {
                    graph: &current.graph,
                },
            )
            .predicted_effect;
            assert!(matches!(
                projection.apply_effect(&effect),
                Err(EffectApplicationError::MountedDescendant(_))
            ));
            assert_eq!(projection, original);
        }
    }

    /// Invalid LUKS names fail before any graph or runtime effect is published.
    #[test]
    fn invalid_luks_mapping_names_leave_projection_unchanged() {
        let (mut current, _) = current_with_disk();
        let container = NodeId::new();
        current
            .graph
            .insert_new(
                container,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::LuksContainer {
                            version: storage_core::model::LuksVersion::Luks2,
                        },
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        for name in ["", "bad/name", "bad\0name"] {
            let mut projection = TestSimulation::from_current(&current);
            let original = projection.clone();
            let result = projection.simulate_action(&ProviderAction::Luks(
                storage_provider::LuksAction::Open {
                    container,
                    planned_node_id: NodeId::new(),
                    mapping_name: name.to_owned(),
                    credential: storage_provider::CredentialRef::from_uuid(container.as_uuid()),
                    read_only: false,
                },
            ));
            assert!(
                matches!(
                    result,
                    Err(ActionSimulationError::Validation { diagnostics })
                        if diagnostics.iter().any(|diagnostic|
                            diagnostic.code == "luks.open.invalid_mapping_name")
                ),
                "invalid mapping name accepted: {name:?}"
            );
            assert_eq!(projection, original);
        }
    }

    /// Builds an unmounted filesystem with one known backing device.
    fn current_with_filesystem(kind: FilesystemKind) -> (CurrentState, NodeId, NodeId) {
        let (mut current, disk) = current_with_disk();
        let filesystem = NodeId::new();
        current
            .graph
            .insert_new(
                filesystem,
                Node {
                    kind: storage_core::model::NodeSpec {
                        kind: NodeKind::Filesystem { kind, label: None },
                        size: Some(storage_core::model::Bytes::new(100)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        current
            .graph
            .insert_dependency(storage_core::model::Dependency {
                from: disk,
                to: filesystem,
                kind: DependencyKind::Backs,
            });
        (current, disk, filesystem)
    }

    /// Mount availability accepts read-only endpoints but never fabricates missing observations.
    #[test]
    fn mount_source_requires_backing_evidence_and_matching_kind() {
        let (mut current, backing, filesystem) = current_with_observed_mapping();
        let condition =
            ActionPrerequisite::MountSourceAvailable(MountSource::Filesystem(filesystem));
        let status = |current: &CurrentState| {
            TestSimulation::from_current(current)
                .evaluate_prerequisites(std::slice::from_ref(&condition))[0]
                .status
                .clone()
        };
        let mut endpoint = current
            .graph
            .node(&backing)
            .unwrap_or_else(|| panic!("missing backing"))
            .clone();
        endpoint
            .size
            .block
            .as_mut()
            .unwrap_or_else(|| panic!("missing block facts"))
            .read_only = Some(true);
        current
            .graph
            .replace_existing(backing, endpoint.clone())
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        assert_eq!(
            status(&current),
            CheckStatus::Satisfied(CheckEvidence::ObservedSnapshot)
        );
        endpoint.size.block = None;
        current
            .graph
            .replace_existing(backing, endpoint)
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        assert!(matches!(status(&current), CheckStatus::Unknown { .. }));
        let mut without_backing = current.clone();
        without_backing
            .graph
            .remove_dependency(&storage_core::model::Dependency {
                from: backing,
                to: filesystem,
                kind: DependencyKind::Backs,
            });
        assert!(matches!(
            status(&without_backing),
            CheckStatus::Unknown { .. }
        ));
        let mut projection = TestSimulation::from_current(&current);
        projection
            .evidence
            .block_interfaces
            .insert(backing, BlockInterfaceAccess::ReadOnly);
        assert_eq!(
            projection.evaluate_prerequisites(std::slice::from_ref(&condition))[0].status,
            CheckStatus::Satisfied(CheckEvidence::PredictedEffect)
        );
        let invalid = ActionPrerequisite::MountSourceAvailable(MountSource::Filesystem(backing));
        assert!(matches!(
            projection.evaluate_prerequisites(&[invalid])[0].status,
            CheckStatus::Violated { .. }
        ));
        let mut source = current
            .graph
            .node(&filesystem)
            .unwrap_or_else(|| panic!("missing filesystem"))
            .clone();
        source.kind.kind = NodeKind::Disk;
        current
            .graph
            .replace_existing(filesystem, source)
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        assert!(matches!(status(&current), CheckStatus::Violated { .. }));
    }

    /// Relabeling rejects read-only backing atomically and preserves its checks across JSON.
    #[test]
    fn filesystem_label_requires_writable_backing() {
        let (mut current, backing, filesystem) = current_with_observed_mapping();
        let mut endpoint = current
            .graph
            .node(&backing)
            .cloned()
            .unwrap_or_else(|| panic!("backing node missing"));
        endpoint
            .size
            .block
            .as_mut()
            .unwrap_or_else(|| panic!("block facts missing"))
            .read_only = Some(true);
        current
            .graph
            .replace_existing(backing, endpoint)
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let action = ProviderAction::Filesystem(FilesystemAction::SetLabel {
            filesystem,
            label: Some("new-label".to_owned()),
        });
        let mut projection = TestSimulation::from_current(&current);
        let original = projection.clone();
        assert!(matches!(
            projection.simulate_action(&action),
            Err(ActionSimulationError::PrerequisitesViolated { .. })
        ));
        assert_eq!(projection, original);
        let context = ActionDescriptionContext {
            graph: &current.graph,
        };
        let trusted = describe_action(&action, context);
        assert!(
            trusted
                .affected_resources
                .iter()
                .any(
                    |access| access.resource == storage_provider::ActionResource::Node(backing)
                        && access.mode == storage_provider::ResourceAccessMode::Write
                )
        );
        let json = serde_json::to_string(&trusted)
            .unwrap_or_else(|error| panic!("serialize label contract: {error}"));
        let mut imported: ActionContract = serde_json::from_str(&json)
            .unwrap_or_else(|error| panic!("deserialize label contract: {error}"));
        assert_eq!(
            storage_provider::verify_imported_contract(&action, imported.clone(), context),
            Ok(trusted)
        );
        imported
            .prerequisites
            .retain(|condition| condition != &ActionPrerequisite::Writable(backing));
        assert!(storage_provider::verify_imported_contract(&action, imported, context).is_err());
    }

    /// Serialized mount contracts retain backing checks and reject weakened imports.
    #[test]
    fn mount_backing_contract_survives_serialization_and_rejects_tampering() {
        let (current, backing, filesystem) = current_with_observed_mapping();
        let action = ProviderAction::Mount(storage_provider::MountAction::Mount {
            source: MountSource::Filesystem(filesystem),
            target: PathBuf::from("/data"),
            options: vec!["ro".to_owned()],
            context: MountContext::Host,
        });
        let context = ActionDescriptionContext {
            graph: &current.graph,
        };
        let contract = describe_action(&action, context);
        assert!(
            contract
                .affected_resources
                .iter()
                .any(
                    |access| access.resource == storage_provider::ActionResource::Node(backing)
                        && access.mode == storage_provider::ResourceAccessMode::Read
                )
        );
        for ancestor in current
            .graph
            .dependencies()
            .filter(|dependency| dependency.to == backing)
            .map(|dependency| dependency.from)
        {
            assert!(
                contract
                    .affected_resources
                    .iter()
                    .any(|access| access.resource
                        == storage_provider::ActionResource::Node(ancestor)
                        && access.mode == storage_provider::ResourceAccessMode::Read)
            );
        }
        let json = serde_json::to_string(&contract)
            .unwrap_or_else(|error| panic!("serialize mount contract: {error}"));
        let mut imported: ActionContract = serde_json::from_str(&json)
            .unwrap_or_else(|error| panic!("deserialize mount contract: {error}"));
        assert_eq!(
            storage_provider::verify_imported_contract(&action, imported.clone(), context),
            Ok(contract.clone())
        );
        imported
            .prerequisites
            .retain(|condition| !matches!(condition, ActionPrerequisite::MountSourceAvailable(_)));
        assert!(storage_provider::verify_imported_contract(&action, imported, context).is_err());
        let mut imported = contract;
        imported
            .affected_resources
            .retain(|access| access.resource != storage_provider::ActionResource::Node(backing));
        assert!(storage_provider::verify_imported_contract(&action, imported, context).is_err());
    }

    /// Mounted ext4 shrink is rejected before publishing a smaller filesystem.
    #[test]
    fn mounted_filesystem_shrink_leaves_projection_unchanged() {
        let (mut current, _, filesystem) = current_with_filesystem(FilesystemKind::Ext4);
        current.mounts.entries.push(ObservedMount {
            source: storage_core::model::MountSource::Filesystem(filesystem),
            target: PathBuf::from("/data"),
            options: vec!["rw".to_owned()],
            context: MountContext::Host,
        });
        let mut projection = TestSimulation::from_current(&current);
        let original = projection.clone();
        let result =
            projection.simulate_action(&ProviderAction::Filesystem(FilesystemAction::Resize {
                filesystem,
                new_size: storage_core::model::Bytes::new(60),
            }));
        assert!(result.is_err());
        assert_eq!(projection, original);
    }

    /// Offline preparation is required in all namespaces and remains a conditional forecast.
    #[test]
    fn shrink_requires_all_unmounts_and_a_forced_clean_check() {
        let (mut current, _, filesystem) = current_with_filesystem(FilesystemKind::Ext4);
        let contexts = [
            MountContext::Host,
            MountContext::Namespace("installer".to_owned()),
        ];
        for context in &contexts {
            current.mounts.entries.push(ObservedMount {
                source: MountSource::Filesystem(filesystem),
                target: PathBuf::from("/data"),
                options: vec!["rw".to_owned()],
                context: context.clone(),
            });
        }
        let baseline = current.clone();
        let mut projection = TestSimulation::from_current(&current);
        let forced_check = ProviderAction::Filesystem(FilesystemAction::Check {
            filesystem,
            force: true,
        });
        let repair = ProviderAction::Filesystem(FilesystemAction::Repair {
            filesystem,
            force: false,
        });
        let resize = ProviderAction::Filesystem(FilesystemAction::Resize {
            filesystem,
            new_size: Bytes::new(60),
        });
        assert!(projection.simulate_action(&forced_check).is_err());
        assert!(projection.simulate_action(&repair).is_err());
        for (index, context) in contexts.into_iter().enumerate() {
            projection
                .simulate_action(&ProviderAction::Mount(
                    storage_provider::MountAction::Unmount {
                        target: PathBuf::from("/data"),
                        context,
                    },
                ))
                .unwrap_or_else(|error| panic!("simulate unmount: {error}"));
            if index == 0 {
                assert!(projection.simulate_action(&resize).is_err());
                assert!(projection.simulate_action(&forced_check).is_err());
            }
        }
        assert!(projection.simulate_action(&resize).is_err());
        projection
            .simulate_action(&ProviderAction::Filesystem(FilesystemAction::Check {
                filesystem,
                force: false,
            }))
            .unwrap_or_else(|error| panic!("simulate non-forced check: {error}"));
        assert!(projection.simulate_action(&resize).is_err());
        let checked = projection
            .simulate_action(&forced_check)
            .unwrap_or_else(|error| panic!("simulate forced check: {error}"));
        assert!(checked.contract.predicted_effect.graph.is_empty());
        assert!(checked.contract.postconditions.contains(
            &storage_provider::Postcondition::FilesystemCheckPassed(filesystem)
        ));
        let resized = projection
            .simulate_action(&resize)
            .unwrap_or_else(|error| panic!("simulate prepared shrink: {error}"));
        assert!(resized.is_conditional());
        assert!(resized.checks.iter().any(|check| matches!(check,
            PrerequisiteCheck { prerequisite: ActionPrerequisite::FilesystemCheckPassed(id), status: CheckStatus::Satisfied(CheckEvidence::PredictedEffect) }
                if *id == filesystem
        )));
        assert!(resized.checks.iter().any(|check| matches!(
            check,
            PrerequisiteCheck {
                prerequisite: ActionPrerequisite::FilesystemMinimumSize { .. },
                status: CheckStatus::Unknown { .. }
            }
        )));
        assert_eq!(current, baseline);
    }

    /// Writes, remounts, and backing geometry changes revoke an earlier clean-check guarantee.
    #[test]
    fn filesystem_check_guarantees_expire_after_changes() {
        let (mut current, disk, filesystem) = current_with_filesystem(FilesystemKind::Ext4);
        let mut backing = current
            .graph
            .node(&disk)
            .cloned()
            .unwrap_or_else(|| panic!("filesystem backing is absent"));
        backing.kind.kind = NodeKind::LvmLv(LvmLv {
            name: "data".to_owned(),
            kind: LvmLvKind::Linear,
        });
        current
            .graph
            .replace_existing(disk, backing)
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let vg = NodeId::new();
        current
            .graph
            .insert_new(
                vg,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::LvmVg {
                            name: "vg0".to_owned(),
                            extent_size: Some(Bytes::new(1)),
                        },
                        size: None,
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        current
            .graph
            .insert_dependency(storage_core::model::Dependency {
                from: vg,
                to: disk,
                kind: DependencyKind::Provides,
            });
        let forced_check = ProviderAction::Filesystem(FilesystemAction::Check {
            filesystem,
            force: true,
        });
        let mut checked = TestSimulation::from_current(&current);
        checked
            .simulate_action(&forced_check)
            .unwrap_or_else(|error| panic!("simulate forced check: {error}"));
        for action in [
            ProviderAction::Lvm(LvmAction::ResizeLv {
                lv: disk,
                new_size: Bytes::new(2048),
            }),
            ProviderAction::Filesystem(FilesystemAction::Repair {
                filesystem,
                force: false,
            }),
            ProviderAction::Filesystem(FilesystemAction::Resize {
                filesystem,
                new_size: Bytes::new(60),
            }),
            ProviderAction::Mount(storage_provider::MountAction::Mount {
                source: MountSource::Filesystem(filesystem),
                target: PathBuf::from("/data"),
                options: vec!["rw".to_owned()],
                context: MountContext::Host,
            }),
        ] {
            let mut projection = checked.clone();
            projection
                .simulate_action(&action)
                .unwrap_or_else(|error| panic!("simulate check invalidation: {error}"));
            let checks = projection
                .evaluate_prerequisites(&[ActionPrerequisite::FilesystemCheckPassed(filesystem)]);
            assert!(matches!(checks[0].status, CheckStatus::Violated { .. }));
        }
    }

    /// Unsupported recipes and zero capacity are rejected; unknown direction stays conservative.
    #[test]
    fn filesystem_resize_rejects_unsupported_and_unknown_shrink_without_preparation() {
        for kind in [
            FilesystemKind::Xfs,
            FilesystemKind::Other("custom".to_owned()),
        ] {
            let (current, _, filesystem) = current_with_filesystem(kind);
            let mut projection = TestSimulation::from_current(&current);
            assert!(
                matches!(projection.simulate_action(&ProviderAction::Filesystem(FilesystemAction::Resize {
                    filesystem, new_size: Bytes::new(60),
                })), Err(ActionSimulationError::Validation { diagnostics })
                    if diagnostics.iter().any(|diagnostic| diagnostic.code == "filesystem.resize.unsupported")
                )
            );
        }
        let (mut current, _, filesystem) = current_with_filesystem(FilesystemKind::Ext4);
        let mut projection = TestSimulation::from_current(&current);
        assert!(
            matches!(projection.simulate_action(&ProviderAction::Filesystem(FilesystemAction::Resize {
                filesystem, new_size: Bytes::new(0),
            })), Err(ActionSimulationError::Validation { diagnostics })
                if diagnostics.iter().any(|diagnostic| diagnostic.code == "filesystem.resize.zero_size")
            )
        );
        let mut node = current
            .graph
            .node(&filesystem)
            .cloned()
            .unwrap_or_else(|| panic!("filesystem is absent"));
        node.kind.size = None;
        current
            .graph
            .replace_existing(filesystem, node)
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        assert!(
            TestSimulation::from_current(&current)
                .simulate_action(&ProviderAction::Filesystem(FilesystemAction::Resize {
                    filesystem,
                    new_size: Bytes::new(60),
                }))
                .is_err()
        );
    }

    /// Builds a current state containing one backing disk.
    fn current_with_disk() -> (CurrentState, NodeId) {
        let disk = NodeId::new();
        let mut graph = NodeGraph::new();
        graph
            .insert_new(
                disk,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::Disk,
                        size: Some(Bytes::new(1024)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        (
            CurrentState {
                graph,
                mounts: MountState::default(),
                environment: Environment {
                    host: SystemEnvironment::default(),
                    target: None,
                },
                diagnostics: Vec::<Diagnostic>::new(),
            },
            disk,
        )
    }

    /// A trusted create effect changes only the isolated projection.
    #[test]
    fn filesystem_create_does_not_mutate_current_state() {
        let (current, disk) = current_with_disk();
        let original = current.clone();
        let filesystem = NodeId::new();
        let action = ProviderAction::Filesystem(FilesystemAction::Create {
            backing: disk,
            planned_node_id: filesystem,
            filesystem: FilesystemKind::Ext4,
            label: Some("data".to_owned()),
        });
        let mut projection = TestSimulation::from_current(&current);

        let result = projection
            .simulate_action(&action)
            .unwrap_or_else(|error| panic!("simulate filesystem create: {error}"));
        assert!(result.is_conditional());
        assert_eq!(current, original);
        let projected = projection
            .graph()
            .node(&filesystem)
            .unwrap_or_else(|| panic!("projected filesystem is missing"));
        assert_eq!(projected.size, NodeFacts::default());
        assert!(matches!(
            &projected.kind.kind,
            NodeKind::Filesystem {
                kind: FilesystemKind::Ext4,
                label: Some(label),
            } if label == "data"
        ));
    }

    /// A failed effect leaves the previously published projection unchanged.
    #[test]
    fn effect_application_is_atomic() {
        let (current, _) = current_with_disk();
        let mut projection = TestSimulation::from_current(&current);
        let original = projection.clone();
        let inserted = NodeId::new();
        let missing = NodeId::new();
        let effect = PredictedEffect {
            graph: vec![
                GraphEffect::InsertNode {
                    id: inserted,
                    spec: NodeSpec {
                        kind: NodeKind::Disk,
                        size: None,
                    },
                },
                GraphEffect::SetNodeSize {
                    node: missing,
                    size: Bytes::new(10),
                },
            ],
            mounts: Vec::new(),
            allocations: Vec::new(),
        };

        assert_eq!(
            projection.apply_effect(&effect),
            Err(EffectApplicationError::MissingNode(missing))
        );
        assert_eq!(projection, original);
    }

    /// Runtime and persistent mount effects remain independent.
    #[test]
    fn mount_effects_keep_runtime_and_persistent_state_separate() {
        let (current, disk) = current_with_disk();
        let mut projection = TestSimulation::from_current(&current);
        let target = PathBuf::from("/data");
        let entry = MountEntry {
            source: MountSource::Filesystem(disk),
            target: target.clone(),
            options: vec!["defaults".to_owned()],
            context: MountContext::Host,
        };
        let effect = PredictedEffect {
            graph: Vec::new(),
            mounts: vec![MountEffect::SetPersistent(entry.clone())],
            allocations: Vec::new(),
        };

        assert!(projection.apply_effect(&effect).is_ok());
        assert!(projection.runtime_mounts().is_empty());
        assert_eq!(projection.persistent_mounts(), &[entry]);
    }

    /// Missing writable observations remain unknown rather than successful.
    #[test]
    fn writable_requires_observed_evidence() {
        let (current, disk) = current_with_disk();
        let projection = TestSimulation::from_current(&current);
        let checks = projection.evaluate_prerequisites(&[ActionPrerequisite::Writable(disk)]);

        assert!(matches!(
            checks.as_slice(),
            [PrerequisiteCheck {
                status: CheckStatus::Unknown { .. },
                ..
            }]
        ));
    }

    /// A known prerequisite violation prevents projection of the action effect.
    #[test]
    fn violated_prerequisite_blocks_action_effect() {
        let (current, _) = current_with_disk();
        let missing = NodeId::new();
        let action = ProviderAction::Lvm(LvmAction::SetActivation {
            target: missing,
            active: true,
        });
        let mut projection = TestSimulation::from_current(&current);
        let original = projection.clone();

        assert!(matches!(
            projection.simulate_action(&action),
            Err(ActionSimulationError::PrerequisitesViolated { .. })
        ));
        assert_eq!(projection, original);
    }

    /// A verified activation guarantee satisfies a later writable requirement.
    #[test]
    fn activation_provides_projected_writable_evidence() {
        let (mut current, _) = current_with_disk();
        let lv = NodeId::new();
        current
            .graph
            .insert_new(
                lv,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::LvmLv(LvmLv {
                            name: "data".to_owned(),
                            kind: LvmLvKind::Linear,
                        }),
                        size: Some(Bytes::new(1024)),
                    },
                    size: NodeFacts::default(),
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let action = ProviderAction::Lvm(LvmAction::SetActivation {
            target: lv,
            active: true,
        });
        let mut projection = TestSimulation::from_current(&current);

        projection
            .simulate_action(&action)
            .unwrap_or_else(|error| panic!("simulate LV activation: {error}"));
        let checks = projection.evaluate_prerequisites(&[ActionPrerequisite::Writable(lv)]);

        assert_eq!(
            checks[0].status,
            CheckStatus::Satisfied(CheckEvidence::PredictedEffect)
        );
        assert_eq!(
            projection.block_interface(&lv),
            Some(BlockInterfaceAccess::ReadWrite)
        );
    }

    /// Deactivation revokes both predicted and previously observed write access.
    #[test]
    fn deactivation_blocks_filesystem_create_until_reactivated() {
        let (mut current, _) = current_with_disk();
        let lv = NodeId::new();
        current
            .graph
            .insert_new(
                lv,
                Node {
                    kind: NodeSpec {
                        kind: NodeKind::LvmLv(LvmLv {
                            name: "data".to_owned(),
                            kind: LvmLvKind::Linear,
                        }),
                        size: Some(Bytes::new(1024)),
                    },
                    size: NodeFacts {
                        block: Some(storage_core::model::BlockFacts {
                            paths: vec![PathBuf::from("/dev/mapper/data")],
                            devno: storage_core::model::DeviceNumber {
                                major: 253,
                                minor: 0,
                            },
                            geometry: None,
                            read_only: Some(false),
                        }),
                        ..NodeFacts::default()
                    },
                },
            )
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        let baseline = current.clone();
        let mut projection = TestSimulation::from_current(&current);
        for active in [true, false] {
            projection
                .simulate_action(&ProviderAction::Lvm(LvmAction::SetActivation {
                    target: lv,
                    active,
                }))
                .unwrap_or_else(|error| panic!("simulate LV activation {active}: {error}"));
        }
        assert_eq!(projection.activation(&lv), Some(false));
        assert_eq!(projection.block_interface(&lv), None);
        let checks = projection.evaluate_prerequisites(&[ActionPrerequisite::Writable(lv)]);
        assert!(matches!(checks[0].status, CheckStatus::Violated { .. }));

        let create = ProviderAction::Filesystem(FilesystemAction::Create {
            backing: lv,
            planned_node_id: NodeId::new(),
            filesystem: FilesystemKind::Ext4,
            label: None,
        });
        let before_create = projection.clone();
        assert!(matches!(
            projection.simulate_action(&create),
            Err(ActionSimulationError::PrerequisitesViolated { .. })
        ));
        assert_eq!(projection, before_create);

        projection
            .simulate_action(&ProviderAction::Lvm(LvmAction::SetActivation {
                target: lv,
                active: true,
            }))
            .unwrap_or_else(|error| panic!("simulate LV reactivation: {error}"));
        projection.simulate_action(&create).unwrap_or_else(|error| {
            panic!("simulate filesystem create after reactivation: {error}")
        });
        assert_eq!(current, baseline);
    }

    /// A projected read-only interface is a known writable violation.
    #[test]
    fn read_only_output_rejects_writable_requirement() {
        let (current, disk) = current_with_disk();
        let mut projection = TestSimulation::from_current(&current);
        projection.apply_outputs(&[ActionOutput::BlockInterface {
            node: disk,
            access: BlockInterfaceAccess::ReadOnly,
        }]);

        let checks = projection.evaluate_prerequisites(&[ActionPrerequisite::Writable(disk)]);

        assert!(matches!(
            checks.as_slice(),
            [PrerequisiteCheck {
                status: CheckStatus::Violated { .. },
                ..
            }]
        ));
    }

    /// Missing provider capacity evidence remains an explicit unknown.
    #[test]
    fn capacity_is_unknown_without_provider_evidence() {
        let (current, disk) = current_with_disk();
        let projection = TestSimulation::from_current(&current);
        let checks = projection.evaluate_prerequisites(&[ActionPrerequisite::CapacityAvailable {
            domain: AllocationDomain::LvmVolumeGroup(disk),
            amount: Some(AllocationQuantity::new(1)),
        }]);

        assert!(matches!(
            checks.as_slice(),
            [PrerequisiteCheck {
                status: CheckStatus::Unknown { .. },
                ..
            }]
        ));
    }
    /// Structural failures at batch completion cannot publish any earlier effect.
    #[test]
    fn structural_validation_is_atomic_and_allows_intermediate_dangling_edges() {
        let (current, backing, filesystem) = current_with_observed_mapping();
        let edge = storage_core::model::Dependency {
            from: backing,
            to: filesystem,
            kind: DependencyKind::Backs,
        };
        let mut projection = TestSimulation::from_current(&current);
        let original = projection.clone();
        let cycle = PredictedEffect {
            graph: vec![
                GraphEffect::SetNodeSize {
                    node: backing,
                    size: Bytes::new(42),
                },
                GraphEffect::InsertDependency(storage_core::model::Dependency {
                    from: filesystem,
                    to: backing,
                    kind: DependencyKind::Backs,
                }),
            ],
            mounts: Vec::new(),
            allocations: Vec::new(),
        };
        assert_eq!(
            projection.apply_effect(&cycle),
            Err(EffectApplicationError::Structure(
                storage_core::model::GraphError::DependencyCycle
            ))
        );
        assert_eq!(projection, original);
        let removal = PredictedEffect {
            graph: vec![
                GraphEffect::RemoveNode(filesystem),
                GraphEffect::RemoveDependency(edge),
            ],
            mounts: Vec::new(),
            allocations: Vec::new(),
        };
        assert!(projection.apply_effect(&removal).is_ok());
        assert!(projection.graph().node(&filesystem).is_none());
        assert_eq!(projection.graph().validate_structure(), Ok(()));
    }
}
