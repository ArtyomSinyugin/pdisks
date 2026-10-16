//! Synthetic post-backend snapshots test verification without invoking storage operations.

use std::error::Error;
use storage_core::model::{Bytes, NodeId, NodeKind, ProbeEpoch};
use storage_engine::{
    HistoricalBaseline, LiveCheckStatus, LiveObservation, ObservationReference,
    live::check_observed_postconditions,
};
use storage_provider::Postcondition;
use uuid::Uuid;

/// Renumbers all existing objects while keeping their provider UUIDs and typed references intact.
fn fixture() -> Result<(HistoricalBaseline, LiveObservation), Box<dyn Error>> {
    let baseline: HistoricalBaseline =
        serde_json::from_str(include_str!("../examples/fixtures/resize_baseline.json"))?;
    let mut encoded = serde_json::to_string(&baseline.snapshot)?;
    for (id, _) in baseline.snapshot.graph.nodes() {
        encoded = encoded.replace(
            &id.as_uuid().to_string(),
            &NodeId::new().as_uuid().to_string(),
        );
    }
    Ok((
        baseline,
        LiveObservation {
            reference: ObservationReference {
                probe_instance: Uuid::now_v7(),
                epoch: ProbeEpoch::new(2),
            },
            snapshot: serde_json::from_str(&encoded)?,
        },
    ))
}

/// Chooses fixture nodes by kind; tested requirements retain the original typed identities.
fn selected(baseline: &HistoricalBaseline) -> Result<(NodeId, NodeId), Box<dyn Error>> {
    let lv = *baseline
        .snapshot
        .graph
        .nodes()
        .find(|(_, node)| matches!(node.kind.kind, NodeKind::LvmLv(_)))
        .ok_or("LV")?
        .0;
    let fs = *baseline
        .snapshot
        .graph
        .nodes()
        .find(|(_, node)| matches!(node.kind.kind, NodeKind::Filesystem { .. }))
        .ok_or("FS")?
        .0;
    Ok((lv, fs))
}

/// Size and label checks inspect fresh correlated objects and retain the exact condition/reference.
#[test]
fn existing_properties_are_verified_by_uuid_in_the_actual_snapshot() -> Result<(), Box<dyn Error>> {
    let (baseline, observation) = fixture()?;
    let (lv, fs) = selected(&baseline)?;
    let spec = baseline.snapshot.graph.node(&lv).ok_or("LV")?.kind.clone();
    let conditions = [
        Postcondition::NodeSize {
            node: lv,
            size: Bytes::new(128 * 1024 * 1024),
        },
        Postcondition::NodeMatches { id: lv, spec },
        Postcondition::NodeReprobed(lv),
        Postcondition::FilesystemLabel {
            filesystem: fs,
            label: Some("data".into()),
        },
    ];
    let report = check_observed_postconditions(&baseline, &observation, &conditions)?;
    assert_eq!(report.observation, observation.reference);
    assert_eq!(
        report
            .checks
            .iter()
            .map(|check| check.condition.clone())
            .collect::<Vec<_>>(),
        conditions
    );
    assert!(
        report
            .checks
            .iter()
            .all(|check| check.status == LiveCheckStatus::Satisfied)
    );
    let mismatches = [
        Postcondition::NodeSize {
            node: lv,
            size: Bytes::new(256 * 1024 * 1024),
        },
        Postcondition::FilesystemLabel {
            filesystem: fs,
            label: Some("renamed".into()),
        },
    ];
    assert!(
        check_observed_postconditions(&baseline, &observation, &mismatches)?
            .checks
            .iter()
            .all(|check| matches!(check.status, LiveCheckStatus::Violated { .. }))
    );
    Ok(())
}

/// Unknown size, missing identity, planned outputs, and fsck results cannot be inferred from success.
#[test]
fn missing_result_evidence_does_not_verify_a_predicted_effect() -> Result<(), Box<dyn Error>> {
    let (baseline, mut observation) = fixture()?;
    let (lv, fs) = selected(&baseline)?;
    let current = *observation
        .snapshot
        .graph
        .nodes()
        .find(|(_, node)| matches!(node.kind.kind, NodeKind::LvmLv(_)))
        .ok_or("LV")?
        .0;
    let mut node = observation
        .snapshot
        .graph
        .node(&current)
        .ok_or("LV")?
        .clone();
    node.kind.size = None;
    observation
        .snapshot
        .graph
        .replace_existing(current, node.clone())?;
    let condition = Postcondition::NodeSize {
        node: lv,
        size: Bytes::new(128 * 1024 * 1024),
    };
    assert!(matches!(
        check_observed_postconditions(&baseline, &observation, &[condition])?.checks[0].status,
        LiveCheckStatus::Unknown { .. }
    ));
    node.size.identities.clear();
    observation.snapshot.graph.replace_existing(current, node)?;
    let conditions = [
        Postcondition::NodeReprobed(lv),
        Postcondition::NodeReprobed(NodeId::new()),
        Postcondition::FilesystemCheckPassed(fs),
    ];
    assert!(
        check_observed_postconditions(&baseline, &observation, &conditions)?
            .checks
            .iter()
            .all(|check| matches!(check.status, LiveCheckStatus::Unknown { .. }))
    );
    Ok(())
}

/// Present edges and mounts can be proven; an incomplete graph/list cannot prove disappearance.
#[test]
fn relationship_checks_preserve_unknown_absence_and_frozen_identity_space()
-> Result<(), Box<dyn Error>> {
    use storage_core::model::{DependencyKind, MountContext, MountSource};
    let (baseline, mut observation) = fixture()?;
    let (lv, fs) = selected(&baseline)?;
    let edge = baseline
        .snapshot
        .graph
        .dependencies()
        .find(|edge| edge.from == lv && edge.to == fs)
        .ok_or("historical backing")?
        .clone();
    let conditions = [
        Postcondition::DependencyPresent(edge.clone()),
        Postcondition::DependencyAbsent(edge),
        Postcondition::NodeAbsent(lv),
        Postcondition::RuntimeMountPresent {
            source: MountSource::Filesystem(fs),
            target: "/data".into(),
            context: MountContext::Host,
        },
        Postcondition::RuntimeMountAbsent {
            target: "/data".into(),
            context: MountContext::Host,
        },
    ];
    let report = check_observed_postconditions(&baseline, &observation, &conditions)?;
    for index in [0, 3] {
        assert_eq!(report.checks[index].status, LiveCheckStatus::Satisfied);
    }
    for index in [1, 2, 4] {
        assert!(matches!(
            report.checks[index].status,
            LiveCheckStatus::Violated { .. }
        ));
    }
    let actual_edge = observation
        .snapshot
        .graph
        .dependencies()
        .find(|edge| edge.kind == DependencyKind::Backs)
        .ok_or("fresh backing")?
        .clone();
    observation.snapshot.graph.remove_dependency(&actual_edge);
    observation.snapshot.mounts.entries.clear();
    let report = check_observed_postconditions(&baseline, &observation, &conditions)?;
    for index in [0, 1, 3, 4] {
        assert!(matches!(
            report.checks[index].status,
            LiveCheckStatus::Unknown { .. }
        ));
    }
    let mut node = observation
        .snapshot
        .graph
        .node(&actual_edge.from)
        .ok_or("LV")?
        .clone();
    node.size.presence = storage_core::model::Presence::Missing;
    observation
        .snapshot
        .graph
        .replace_existing(actual_edge.from, node)?;
    let absent = [
        Postcondition::NodeAbsent(lv),
        Postcondition::NodeAbsent(NodeId::new()),
        Postcondition::RuntimeMountPresent {
            source: MountSource::Filesystem(fs),
            target: "/data".into(),
            context: MountContext::Namespace("unresolved".into()),
        },
        Postcondition::PersistentMountAbsent {
            target: "/data".into(),
            context: MountContext::Host,
        },
    ];
    assert!(
        check_observed_postconditions(&baseline, &observation, &absent)?
            .checks
            .iter()
            .all(|check| matches!(check.status, LiveCheckStatus::Unknown { .. }))
    );
    Ok(())
}

/// An assembler conflict revokes positive postconditions even if one retained contribution matches.
#[test]
fn blocking_diagnostics_and_invalid_envelopes_cannot_verify_results() -> Result<(), Box<dyn Error>>
{
    use storage_core::model::{Diagnostic, DiagnosticSeverity};
    let (baseline, mut observation) = fixture()?;
    let (lv, _) = selected(&baseline)?;
    observation.snapshot.diagnostics.push(Diagnostic {
        code: "provider.node_conflict".into(),
        severity: DiagnosticSeverity::Error,
        subjects: Vec::new(),
        message: "conflicting result".into(),
        evidence: None,
        suggested_remedy: None,
    });
    let conditions = [
        Postcondition::NodeReprobed(lv),
        Postcondition::NodeSize {
            node: lv,
            size: Bytes::new(1),
        },
    ];
    let report = check_observed_postconditions(&baseline, &observation, &conditions)?;
    assert!(matches!(
        report.checks[0].status,
        LiveCheckStatus::Unknown { .. }
    ));
    assert!(matches!(
        report.checks[1].status,
        LiveCheckStatus::Violated { .. }
    ));
    observation.reference.probe_instance = Uuid::nil();
    assert!(check_observed_postconditions(&baseline, &observation, &conditions).is_err());
    Ok(())
}
