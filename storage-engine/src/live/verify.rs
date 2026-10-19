//! Conservative result checks over one retained actual post-backend observation.

use serde::Serialize;
use storage_core::model::{ExternalId, Node, NodeId, NodeKind, Presence};
use storage_provider::Postcondition;

use super::{ObservationError, correlated_node, qualify_observed_status, unknown};
use crate::{
    HistoricalBaseline, LiveCheck, LiveCheckStatus, LiveObservation, ObservationReference,
    editing::NodeCorrelation,
};

/// Observed postconditions retain frozen identities and the exact publication they inspect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservedPostconditions {
    /// Publication to bind to the runtime's retained completion, never a later substitute.
    pub observation: ObservationReference,
    /// Exactly the requested postconditions in their original order.
    pub checks: Vec<LiveCheck<Postcondition>>,
}

/// Checks supported existing-object results without applying a predicted effect.
/// The trusted host must supply the actual post-backend publication; this does not prove dispatch.
/// Planned outputs, absent coverage, and provider-owned check results remain unknown.
pub fn check_observed_postconditions(
    baseline: &HistoricalBaseline,
    observation: &LiveObservation,
    postconditions: &[Postcondition],
) -> Result<ObservedPostconditions, ObservationError> {
    check_observed_postconditions_with_bindings(baseline, observation, postconditions, &[])
}

/// Checks actual result properties with provisional LV/filesystem identities supplied by the trusted runtime.
/// Bindings establish correlation only: size, spec, dependency and diagnostics are still checked.
/// The runtime must prove a new UUID came from this invocation and has the admitted backing before publishing a binding.
pub fn check_observed_postconditions_with_bindings(
    baseline: &HistoricalBaseline,
    observation: &LiveObservation,
    postconditions: &[Postcondition],
    bindings: &[NodeCorrelation],
) -> Result<ObservedPostconditions, ObservationError> {
    observation.validate()?;
    baseline.snapshot.graph.validate_structure()?;
    Ok(ObservedPostconditions {
        observation: observation.reference,
        checks: postconditions
            .iter()
            .map(|condition| LiveCheck {
                condition: condition.clone(),
                status: qualify_observed_status(
                    observation,
                    check_result(baseline, observation, condition, bindings),
                ),
            })
            .collect(),
    })
}

/// Correlates existing objects before inspecting actual properties; missing matches are not success.
fn check_result(
    baseline: &HistoricalBaseline,
    observation: &LiveObservation,
    condition: &Postcondition,
    bindings: &[NodeCorrelation],
) -> LiveCheckStatus {
    match condition {
        Postcondition::DependencyPresent(edge) | Postcondition::DependencyAbsent(edge) => {
            let (from, _) = match result_node(baseline, observation, edge.from, bindings) {
                Ok(value) => value,
                Err(status) => return status,
            };
            let (to, _) = match result_node(baseline, observation, edge.to, bindings) {
                Ok(value) => value,
                Err(status) => return status,
            };
            let present =
                observation.snapshot.graph.dependencies().any(|actual| {
                    actual.from == from && actual.to == to && actual.kind == edge.kind
                });
            return match (condition, present) {
                (Postcondition::DependencyPresent(_), true) => LiveCheckStatus::Satisfied,
                (Postcondition::DependencyAbsent(_), true) => {
                    violated("dependency is still present in the observed graph")
                }
                _ => unknown("complete dependency coverage is required to establish absence"),
            };
        }
        Postcondition::RuntimeMountPresent {
            source,
            target,
            context,
        } => {
            return super::mount::observed_mount(
                baseline,
                observation,
                source,
                target,
                context,
                bindings,
            )
            .map(|_| LiveCheckStatus::Satisfied)
            .unwrap_or_else(|status| status);
        }
        Postcondition::RuntimeMountAbsent { target, context } => {
            return if observation
                .snapshot
                .mounts
                .entries
                .iter()
                .any(|mount| mount.target == *target && mount.context == *context)
            {
                violated("mount target is still occupied in an observed context")
            } else {
                unknown("complete mount namespace coverage is required to establish absence")
            };
        }
        _ => {}
    }
    let source = match condition {
        Postcondition::NodeMatches { id, .. } => *id,
        Postcondition::NodeSize { node, .. }
        | Postcondition::NodeReprobed(node)
        | Postcondition::Activation { node, .. } => *node,
        Postcondition::FilesystemLabel { filesystem, .. } => *filesystem,
        Postcondition::NodeAbsent(node) => *node,
        _ => {
            return unknown(
                "provider result or complete observation coverage is required for this postcondition",
            );
        }
    };
    let (_, observed) = match result_node(baseline, observation, source, bindings) {
        Ok(value) => value,
        Err(status) => return status,
    };
    match condition {
        Postcondition::Activation { active, .. }
            if matches!(observed.kind.kind, NodeKind::LvmLv(_)) =>
        {
            match observed.size.lvm_active {
                Some(actual) if actual == *active => LiveCheckStatus::Satisfied,
                Some(_) => violated("observed LV activation differs from the requested result"),
                None => unknown("fresh unambiguous native LV activation state is required"),
            }
        }
        Postcondition::NodeAbsent(_) => {
            violated("correlated object is still present in the observation")
        }
        Postcondition::NodeReprobed(_) => LiveCheckStatus::Satisfied,
        Postcondition::NodeSize { size, .. } => match observed.kind.size {
            Some(actual) if actual == *size => LiveCheckStatus::Satisfied,
            Some(_) => violated("observed size differs from the requested result"),
            None => unknown("fresh observed node size is required"),
        },
        Postcondition::NodeMatches { spec, .. } => {
            if observed.kind.kind != spec.kind {
                return violated(
                    "observed node kind or properties differ from the requested result",
                );
            }
            match (spec.size, observed.kind.size) {
                (None, _) => LiveCheckStatus::Satisfied,
                (Some(expected), Some(actual)) if expected == actual => LiveCheckStatus::Satisfied,
                (Some(_), Some(_)) => violated("observed size differs from the requested result"),
                (Some(_), None) => unknown("fresh observed node size is required"),
            }
        }
        Postcondition::FilesystemLabel { label, .. } => match &observed.kind.kind {
            NodeKind::Filesystem { label: actual, .. } if actual == label => {
                LiveCheckStatus::Satisfied
            }
            NodeKind::Filesystem { .. } => {
                violated("observed filesystem label differs from the requested result")
            }
            _ => violated("correlated object is not a filesystem"),
        },
        _ => unknown("unsupported observed node result"),
    }
}

/// Existing-node bindings never override baseline correlation; only unique present LV/FS UUIDs qualify.
pub(super) fn result_node<'a>(
    baseline: &HistoricalBaseline,
    observation: &'a LiveObservation,
    source: NodeId,
    bindings: &[NodeCorrelation],
) -> Result<(NodeId, &'a Node), LiveCheckStatus> {
    if baseline.snapshot.graph.node(&source).is_some() {
        return correlated_node(baseline, observation, source);
    }
    let mut candidates = bindings.iter().filter(|item| item.source == source);
    let binding = candidates
        .next()
        .ok_or_else(|| unknown("created output requires runtime UUID evidence"))?;
    if candidates.next().is_some()
        || bindings
            .iter()
            .filter(|item| item.target == binding.target)
            .count()
            != 1
    {
        return Err(unknown("created output binding is ambiguous"));
    }
    // A new output cannot adopt an identity that already belonged to the reviewed system.
    if baseline
        .snapshot
        .graph
        .nodes()
        .any(|(_, node)| node.size.identities.contains(&binding.identity))
    {
        return Err(unknown(
            "created output UUID already exists in the historical baseline",
        ));
    }
    let node = observation
        .snapshot
        .graph
        .node(&binding.target)
        .ok_or_else(|| unknown("bound output is absent"))?;
    let identity_matches = match (&node.kind.kind, &binding.identity) {
        (NodeKind::LvmLv(_), ExternalId::LvmLvUuid(uuid)) => {
            !uuid.trim().is_empty() && uuid.trim() == uuid
        }
        (NodeKind::Filesystem { kind, .. }, ExternalId::Filesystem { fs_type, value }) => {
            fs_type == kind.as_str() && !value.trim().is_empty() && value.trim() == value
        }
        _ => false,
    };
    if source.as_uuid().is_nil()
        || node.size.presence != Presence::Present
        || !identity_matches
        || !node.size.identities.contains(&binding.identity)
        || node
            .size
            .identities
            .iter()
            .filter(|id| std::mem::discriminant(*id) == std::mem::discriminant(&binding.identity))
            .count()
            != 1
        || observation
            .snapshot
            .graph
            .nodes()
            .filter(|(_, node)| node.size.identities.contains(&binding.identity))
            .count()
            != 1
    {
        return Err(unknown(
            "created output needs one unambiguous observed LV or filesystem UUID",
        ));
    }
    Ok((binding.target, node))
}

/// Reports a concrete mismatch separately from an evidence gap.
fn violated(reason: &str) -> LiveCheckStatus {
    LiveCheckStatus::Violated {
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage_core::model::{LvmLv, LvmLvKind, ProbeEpoch};
    use uuid::Uuid;

    /// Provisional correlation cannot override existing identities or bypass UUID uniqueness.
    #[test]
    fn output_correlations_require_unique_uuid_and_never_override_baseline()
    -> Result<(), Box<dyn std::error::Error>> {
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../../examples/fixtures/probe_snapshot.json"))?;
        let snapshot = serde_json::from_value(raw["current"].clone())?;
        let baseline = HistoricalBaseline {
            id: Uuid::now_v7(),
            snapshot,
            capacity_evidence: Vec::new(),
        };
        let (&vg, original) = baseline.snapshot.graph.nodes().next().ok_or("VG")?;
        let planned = NodeId::new();
        let actual = NodeId::new();
        let mut lv = original.clone();
        lv.kind.kind = NodeKind::LvmLv(LvmLv {
            name: "data".into(),
            kind: LvmLvKind::Linear,
        });
        lv.size.identities = vec![ExternalId::LvmLvUuid("created-lv".into())];
        let mut observation = LiveObservation {
            reference: ObservationReference {
                probe_instance: Uuid::now_v7(),
                epoch: ProbeEpoch::new(1),
            },
            snapshot: baseline.snapshot.clone(),
        };
        observation.snapshot.graph.insert_new(actual, lv.clone())?;
        let binding = NodeCorrelation {
            source: planned,
            target: actual,
            identity: lv.size.identities[0].clone(),
        };
        let condition = Postcondition::NodeReprobed(planned);
        let prerequisites = [storage_provider::ActionPrerequisite::NodeExists(planned)];
        assert!(matches!(
            super::super::check_observed_prerequisites(&baseline, &observation, &prerequisites)?
                .checks[0]
                .status,
            LiveCheckStatus::Unknown { .. }
        ));
        assert_eq!(
            super::super::check_observed_prerequisites_with_bindings(
                &baseline,
                &observation,
                &[],
                &prerequisites,
                std::slice::from_ref(&binding)
            )?
            .checks[0]
                .status,
            LiveCheckStatus::Satisfied
        );
        assert_eq!(
            check_observed_postconditions_with_bindings(
                &baseline,
                &observation,
                std::slice::from_ref(&condition),
                std::slice::from_ref(&binding)
            )?
            .checks[0]
                .status,
            LiveCheckStatus::Satisfied
        );
        for case in 0..4 {
            let mut observed = observation.clone();
            let mut bindings = vec![binding.clone()];
            match case {
                0 => bindings.push(binding.clone()),
                1 => {
                    bindings[0].identity = ExternalId::Serial {
                        vendor: None,
                        value: "not an LV UUID".into(),
                    }
                }
                2 => {
                    observed
                        .snapshot
                        .graph
                        .insert_new(NodeId::new(), lv.clone())?;
                }
                _ => bindings.clear(),
            }
            assert!(matches!(
                super::super::check_observed_prerequisites_with_bindings(
                    &baseline,
                    &observed,
                    &[],
                    &prerequisites,
                    &bindings
                )?
                .checks[0]
                    .status,
                LiveCheckStatus::Unknown { .. }
            ));
            assert!(
                matches!(
                    check_observed_postconditions_with_bindings(
                        &baseline,
                        &observed,
                        std::slice::from_ref(&condition),
                        &bindings
                    )?
                    .checks[0]
                        .status,
                    LiveCheckStatus::Unknown { .. }
                ),
                "case {case}"
            );
        }
        for (active, expected) in [
            (Some(true), "satisfied"),
            (Some(false), "violated"),
            (None, "unknown"),
        ] {
            let mut observed = observation.clone();
            let mut node = lv.clone();
            node.size.lvm_active = active;
            observed.snapshot.graph.replace_existing(actual, node)?;
            let report = check_observed_postconditions_with_bindings(
                &baseline,
                &observed,
                &[Postcondition::Activation {
                    node: planned,
                    active: true,
                }],
                std::slice::from_ref(&binding),
            )?;
            match expected {
                "satisfied" => assert_eq!(report.checks[0].status, LiveCheckStatus::Satisfied),
                "violated" => assert!(matches!(
                    report.checks[0].status,
                    LiveCheckStatus::Violated { .. }
                )),
                _ => assert!(matches!(
                    report.checks[0].status,
                    LiveCheckStatus::Unknown { .. }
                )),
            }
        }
        let mut replaced = original.clone();
        replaced.size.identities = vec![ExternalId::LvmVgUuid("replacement-vg".into())];
        observation.snapshot.graph.replace_existing(vg, replaced)?;
        let forged = NodeCorrelation {
            source: vg,
            target: actual,
            identity: binding.identity,
        };
        assert!(matches!(
            check_observed_postconditions_with_bindings(
                &baseline,
                &observation,
                &[Postcondition::NodeReprobed(vg)],
                &[forged]
            )?
            .checks[0]
                .status,
            LiveCheckStatus::Unknown { .. }
        ));
        Ok(())
    }
}
