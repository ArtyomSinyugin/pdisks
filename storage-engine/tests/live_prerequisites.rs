//! Read-only evidence checks over synthetic probe snapshots; no native backend is invoked.

use std::error::Error;

use storage_core::model::{NodeId, NodeKind, ProbeEpoch};
use storage_engine::{
    HistoricalBaseline, LiveCheckStatus, LiveObservation, ObservationReference,
    live::check_observed_prerequisites,
};
use storage_provider::{ActionPrerequisite, NodeKindClass};
use uuid::Uuid;

/// Renumbers a present LV while retaining its UUID and updating every graph reference.
fn fixture() -> Result<(HistoricalBaseline, LiveObservation, NodeId, NodeId), Box<dyn Error>> {
    let baseline: HistoricalBaseline =
        serde_json::from_str(include_str!("../examples/fixtures/resize_baseline.json"))?;
    let old = *baseline
        .snapshot
        .graph
        .nodes()
        .find(|(_, node)| matches!(node.kind.kind, NodeKind::LvmLv(_)))
        .ok_or("LV fixture")?
        .0;
    let new = NodeId::new();
    let snapshot = serde_json::from_str(
        &serde_json::to_string(&baseline.snapshot)?
            .replace(&old.as_uuid().to_string(), &new.as_uuid().to_string()),
    )?;
    let observation = LiveObservation {
        reference: ObservationReference {
            probe_instance: Uuid::now_v7(),
            epoch: ProbeEpoch::new(1),
        },
        snapshot,
    };
    Ok((baseline, observation, old, new))
}

/// Results use the correlated present endpoint while preserving the frozen condition identities.
#[test]
fn observed_node_facts_follow_uuid_and_fresh_readonly_status() -> Result<(), Box<dyn Error>> {
    let (baseline, mut observation, old, new) = fixture()?;
    let conditions = vec![
        ActionPrerequisite::NodeExists(old),
        ActionPrerequisite::NodeKind {
            node: old,
            expected: NodeKindClass::Block,
        },
        ActionPrerequisite::Writable(old),
    ];
    let report = check_observed_prerequisites(&baseline, &observation, &conditions)?;
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
    let mut node = observation
        .snapshot
        .graph
        .node(&new)
        .ok_or("new LV")?
        .clone();
    node.size.block.as_mut().ok_or("block facts")?.read_only = Some(true);
    observation
        .snapshot
        .graph
        .replace_existing(new, node.clone())?;
    let report = check_observed_prerequisites(&baseline, &observation, &conditions)?;
    assert!(matches!(
        report.checks[2].status,
        LiveCheckStatus::Violated { .. }
    ));
    node.size.block.as_mut().ok_or("block facts")?.read_only = None;
    observation.snapshot.graph.replace_existing(new, node)?;
    let report = check_observed_prerequisites(&baseline, &observation, &conditions)?;
    assert!(matches!(
        report.checks[2].status,
        LiveCheckStatus::Unknown { .. }
    ));
    assert_eq!(
        baseline
            .snapshot
            .graph
            .node(&old)
            .ok_or("old LV")?
            .size
            .block
            .as_ref()
            .ok_or("block")?
            .read_only,
        Some(false)
    );
    Ok(())
}

/// Equality of IDs and paths cannot repair missing, conflicting, or non-present identities.
#[test]
fn correlation_gaps_and_external_permissions_remain_unknown() -> Result<(), Box<dyn Error>> {
    let (baseline, observation, old, new) = fixture()?;
    for case in 0..4 {
        let mut observed = observation.clone();
        let mut node = observed.snapshot.graph.node(&new).ok_or("LV")?.clone();
        match case {
            0 => node.size.identities.clear(),
            1 => node.size.presence = storage_core::model::Presence::Unknown,
            2 => node.size.identities.push(node.size.identities[0].clone()),
            _ => {
                observed
                    .snapshot
                    .graph
                    .insert_new(NodeId::new(), node.clone())?;
            }
        }
        observed.snapshot.graph.replace_existing(new, node)?;
        let report = check_observed_prerequisites(
            &baseline,
            &observed,
            &[ActionPrerequisite::NodeExists(old)],
        )?;
        assert!(matches!(
            report.checks[0].status,
            LiveCheckStatus::Unknown { .. }
        ));
    }
    let mut same_ids = observation.clone();
    same_ids.snapshot = baseline.snapshot.clone();
    let mut replacement = same_ids.snapshot.graph.node(&old).ok_or("old LV")?.clone();
    replacement.size.identities.clear();
    same_ids.snapshot.graph.replace_existing(old, replacement)?;
    let report =
        check_observed_prerequisites(&baseline, &same_ids, &[ActionPrerequisite::NodeExists(old)])?;
    assert!(matches!(
        report.checks[0].status,
        LiveCheckStatus::Unknown { .. }
    ));
    let conditions = [
        ActionPrerequisite::NodeExists(NodeId::new()),
        ActionPrerequisite::DestructiveApproval(old),
        ActionPrerequisite::FilesystemCheckPassed(old),
    ];
    let report = check_observed_prerequisites(&baseline, &observation, &conditions)?;
    assert!(
        report
            .checks
            .iter()
            .all(|check| matches!(check.status, LiveCheckStatus::Unknown { .. }))
    );
    Ok(())
}
