//! Conversion of same-pass raw LVM observations at the application-to-engine boundary.

use std::collections::HashMap;

use storage_core::model::{
    CurrentState, Diagnostic, DiagnosticSeverity, DiagnosticSubject, ExternalId, NodeId, NodeKind,
    Presence,
};
use storage_provider::{AllocationDomain, AllocationQuantity, LvmVgObservation};
use uuid::Uuid;

use crate::{CapacityEvidence, HistoricalBaseline, PlanSimulationError};

impl HistoricalBaseline {
    /// Binds a consumed probe state and its same-pass records to a fresh historical context.
    /// Invalid, ambiguous, exported, or imprecise capacity stays unknown with a diagnostic.
    /// The caller must pass records from this publication, never a later independent measurement.
    pub fn from_lvm_observations(
        mut snapshot: CurrentState,
        observations: &[LvmVgObservation],
    ) -> Result<Self, PlanSimulationError> {
        snapshot.graph.validate_structure()?;
        let mut nodes = HashMap::<NodeId, usize>::new();
        let mut uuids = HashMap::<&str, usize>::new();
        for observation in observations {
            *nodes.entry(observation.node).or_default() += 1;
            *uuids.entry(&observation.vg.uuid).or_default() += 1;
        }
        let mut graph_uuids = HashMap::<&str, Vec<NodeId>>::new();
        for (id, node) in snapshot.graph.nodes() {
            for identity in &node.size.identities {
                if let ExternalId::LvmVgUuid(uuid) = identity {
                    graph_uuids.entry(uuid).or_default().push(*id);
                }
            }
        }
        let mut capacity_evidence = Vec::new();
        for observation in observations {
            let vg = &observation.vg;
            let matched_node = snapshot.graph.node(&observation.node).is_some_and(|node| {
                matches!(&node.kind.kind, NodeKind::LvmVg { name, extent_size: Some(extent) }
                    if name == &vg.name && *extent == storage_core::model::Bytes::new(vg.extent_size))
                    && node.kind.size.is_some_and(|size| size == storage_core::model::Bytes::new(vg.size))
                    && node.size.presence == Presence::Present
                    && node
                        .size
                        .identities
                        .iter()
                        .filter(|identity| matches!(identity, ExternalId::LvmVgUuid(_)))
                        .count()
                        == 1
            });
            let exact = vg.extent_size != 0
                && vg.free <= vg.size
                && vg.free.is_multiple_of(vg.extent_size)
                && vg.size.is_multiple_of(vg.extent_size);
            let unique = nodes.get(&observation.node) == Some(&1)
                && uuids.get(vg.uuid.as_str()) == Some(&1)
                && graph_uuids
                    .get(vg.uuid.as_str())
                    .is_some_and(|ids| ids.as_slice() == [observation.node]);
            if matched_node
                && exact
                && unique
                && !vg.exported
                && !vg.uuid.trim().is_empty()
                && vg.uuid.trim() == vg.uuid
            {
                capacity_evidence.push(CapacityEvidence {
                    domain: AllocationDomain::LvmVolumeGroup(observation.node),
                    available: AllocationQuantity::new(vg.free / vg.extent_size),
                });
            } else {
                snapshot.diagnostics.push(Diagnostic {
                    code: "baseline.lvm_capacity_unknown".to_owned(),
                    severity: DiagnosticSeverity::MissingInformation,
                    subjects: vec![DiagnosticSubject::Node(observation.node)],
                    message:
                        "LVM capacity cannot be bound uniquely and exactly to this observed VG"
                            .to_owned(),
                    evidence: Some(format!(
                        "uuid={:?}, size={}, free={}, extent_size={}, exported={}",
                        vg.uuid, vg.size, vg.free, vg.extent_size, vg.exported
                    )),
                    suggested_remedy: Some(
                        "repeat the complete probe; inspect VG identity and extent measurements"
                            .to_owned(),
                    ),
                });
            }
        }
        Ok(Self {
            id: Uuid::now_v7(),
            snapshot,
            capacity_evidence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editing::{PlanSession, PreviewStatus};
    use storage_core::model::{Bytes, NodeFacts};
    use storage_provider::LvmVgEntry;

    /// Builds a present VG whose measured geometry agrees with its raw record.
    fn fixture() -> (CurrentState, LvmVgObservation) {
        let baseline: HistoricalBaseline =
            serde_json::from_str(include_str!("../tests/fixtures/editing_baseline.json"))
                .unwrap_or_else(|error| panic!("fixture: {error}"));
        let AllocationDomain::LvmVolumeGroup(node) = baseline.capacity_evidence[0].domain;
        let mut current = baseline.snapshot;
        let mut vg = current
            .graph
            .node(&node)
            .cloned()
            .unwrap_or_else(|| panic!("VG node"));
        vg.kind.size = Some(Bytes::new(400 * 1024 * 1024));
        vg.size = NodeFacts {
            presence: Presence::Present,
            identities: vec![ExternalId::LvmVgUuid("vg-uuid".to_owned())],
            ..NodeFacts::default()
        };
        current
            .graph
            .replace_existing(node, vg)
            .unwrap_or_else(|error| panic!("replace VG: {error}"));
        (
            current,
            LvmVgObservation {
                node,
                vg: LvmVgEntry {
                    name: "vg0".to_owned(),
                    uuid: "vg-uuid".to_owned(),
                    size: 400 * 1024 * 1024,
                    extent_size: 4 * 1024 * 1024,
                    free: 200 * 1024 * 1024,
                    pv_count: 1,
                    exported: false,
                },
            },
        )
    }

    /// Conversion uses exact extents and preserves the distinction between unknown and known zero.
    #[test]
    fn exact_capacity_and_zero_are_bound_to_the_consumed_snapshot() {
        let (current, mut observation) = fixture();
        let original = current.clone();
        for (free, expected) in [(200 * 1024 * 1024, 50), (0, 0)] {
            observation.vg.free = free;
            let baseline =
                HistoricalBaseline::from_lvm_observations(current.clone(), &[observation.clone()])
                    .unwrap_or_else(|error| panic!("baseline: {error}"));
            assert_eq!(baseline.snapshot, original);
            assert_eq!(baseline.capacity_evidence[0].available.get(), expected);
            assert_eq!(
                baseline.capacity_evidence[0].domain,
                AllocationDomain::LvmVolumeGroup(observation.node)
            );
            let mut session =
                PlanSession::new(baseline).unwrap_or_else(|error| panic!("session: {error}"));
            session
                .add_lv(
                    session.revision(),
                    observation.node,
                    "data",
                    Bytes::new((expected + 1) * observation.vg.extent_size),
                )
                .unwrap_or_else(|error| panic!("edit: {error}"));
            assert_eq!(session.preview().status(), PreviewStatus::Invalid);
        }
        let baseline = HistoricalBaseline::from_lvm_observations(current, &[])
            .unwrap_or_else(|error| panic!("no observations: {error}"));
        assert!(baseline.capacity_evidence.is_empty());
        let mut session =
            PlanSession::new(baseline).unwrap_or_else(|error| panic!("session: {error}"));
        session
            .add_lv(
                session.revision(),
                observation.node,
                "data",
                Bytes::new(4 * 1024 * 1024),
            )
            .unwrap_or_else(|error| panic!("edit: {error}"));
        assert_eq!(session.preview().status(), PreviewStatus::Conditional);
    }

    /// Any mismatch or imprecise measurement leaves this domain unknown rather than rounded.
    #[test]
    fn invalid_or_unmatched_observations_never_become_capacity_evidence() {
        for case in 0..13 {
            let (mut current, mut observation) = fixture();
            let mut node = current
                .graph
                .node(&observation.node)
                .cloned()
                .unwrap_or_else(|| panic!("VG"));
            match case {
                0 => observation.vg.extent_size = 0,
                1 => observation.vg.free += 1,
                2 => observation.vg.free = u64::MAX,
                3 => observation.vg.size += 1,
                4 => observation.vg.exported = true,
                5 => observation.vg.uuid = "different-uuid".to_owned(),
                6 => {
                    observation.vg.uuid = " ".to_owned();
                    node.size.identities = vec![ExternalId::LvmVgUuid(" ".to_owned())];
                }
                7 => node.kind.kind = NodeKind::Disk,
                8 => node.kind.size = None,
                9 => node.size.presence = Presence::Missing,
                10 => node
                    .size
                    .identities
                    .push(ExternalId::LvmVgUuid("other".to_owned())),
                11 => observation.vg.extent_size *= 2,
                _ => observation.vg.name = "renamed".to_owned(),
            }
            current
                .graph
                .replace_existing(observation.node, node)
                .unwrap_or_else(|error| panic!("replace: {error}"));
            let baseline = HistoricalBaseline::from_lvm_observations(current, &[observation])
                .unwrap_or_else(|error| panic!("baseline: {error}"));
            assert!(baseline.capacity_evidence.is_empty(), "case {case}");
            assert!(
                baseline
                    .snapshot
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.code == "baseline.lvm_capacity_unknown")
            );
        }
        let (current, mut observation) = fixture();
        observation.node = NodeId::new();
        assert!(
            HistoricalBaseline::from_lvm_observations(current, &[observation])
                .unwrap_or_else(|error| panic!("absent: {error}"))
                .capacity_evidence
                .is_empty()
        );
    }

    /// Duplicate records and cloned persistent identities invalidate all ambiguous candidates.
    #[test]
    fn duplicate_domains_and_uuids_are_not_silently_deduplicated() {
        let (current, observation) = fixture();
        let baseline = HistoricalBaseline::from_lvm_observations(
            current.clone(),
            &[observation.clone(), observation.clone()],
        )
        .unwrap_or_else(|error| panic!("duplicates: {error}"));
        assert!(baseline.capacity_evidence.is_empty());
        let mut current = current;
        let second = NodeId::new();
        let node = current
            .graph
            .node(&observation.node)
            .cloned()
            .unwrap_or_else(|| panic!("VG"));
        current
            .graph
            .insert_new(second, node)
            .unwrap_or_else(|error| panic!("insert cloned VG: {error}"));
        let baseline = HistoricalBaseline::from_lvm_observations(current, &[observation])
            .unwrap_or_else(|error| panic!("cloned UUID: {error}"));
        assert!(baseline.capacity_evidence.is_empty());
    }
}
