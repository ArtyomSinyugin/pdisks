//! Same-pass probe publications and conservative live LVM capacity checks.

use storage_core::model::{
    DependencyKind, ExternalId, MemberRole, NodeId, NodeKind, Presence, ProbeEpoch,
};
use storage_probe::LinuxProbe;
use storage_provider::{ActionPrerequisite, AllocationDomain};
use thiserror::Error;

use storage_engine::{
    CapacityEvidence, HistoricalBaseline, LiveCheckStatus, LiveObservation, ObservationReference,
    PlanSimulationError,
    live::{ObservationError, ObservedPrerequisites, check_observed_prerequisites_with_capacity},
};

/// Failed collection supplies neither a runtime publication nor positive evidence.
#[derive(Debug, Error)]
pub enum ProbeObservationError {
    /// Graph and same-pass capacity could not be interpreted safely.
    #[error(transparent)]
    Capacity(#[from] PlanSimulationError),
    /// The publication envelope is malformed.
    #[error(transparent)]
    Observation(#[from] ObservationError),
}

/// One provider pass, collected directly rather than imported from saved JSON.
/// Adapter selection remains the trusted host's responsibility; fixtures remain synthetic.
#[derive(Debug)]
pub struct ProbedObservation {
    /// Actual graph and diagnostics, including rejected same-pass allocation measurements.
    observation: LiveObservation,
    /// Validated allocation counts from exactly the provider calls that produced this graph.
    capacity: Vec<CapacityEvidence>,
    /// Raw PV counts from the exact VG publications; usable only alongside validated capacity.
    vg_members: Vec<(NodeId, u64)>,
}

impl ProbedObservation {
    /// Calls the selected probe once and binds graph/capacity to its instance and generation.
    /// No predicted graph or historical free-space values enter this publication.
    pub fn collect(probe: &LinuxProbe<'_>) -> Result<Self, ProbeObservationError> {
        let publication = probe.probe();
        let vg_members = publication
            .lvm_vgs
            .iter()
            .map(|vg| (vg.node, vg.vg.pv_count))
            .collect();
        let measured =
            HistoricalBaseline::from_lvm_observations(publication.current, &publication.lvm_vgs)?;
        let observation = LiveObservation {
            reference: ObservationReference {
                probe_instance: probe.instance_id(),
                epoch: ProbeEpoch::new(publication.generation),
            },
            snapshot: measured.snapshot,
        };
        observation.validate()?;
        Ok(Self {
            observation,
            capacity: measured.capacity_evidence,
            vg_members,
        })
    }

    /// Borrows the immutable publication used by every check on this value.
    pub fn observation(&self) -> &LiveObservation {
        &self.observation
    }

    /// Captures graph and validated allocation measurements from this exact pass as editing history.
    /// This consumes provenance; the saved baseline grants no future live execution permission.
    pub fn into_baseline(self) -> HistoricalBaseline {
        HistoricalBaseline {
            id: uuid::Uuid::now_v7(),
            snapshot: self.observation.snapshot,
            capacity_evidence: self.capacity,
        }
    }

    /// Transfers actual state to the runtime after all checks have used this same publication.
    pub fn into_observation(self) -> LiveObservation {
        self.observation
    }

    /// Requires a complete UUID-qualified PV membership consistent with same-pass VG measurements.
    /// This proves only observed topology, not global namespace coverage or native backend availability.
    pub fn check_vg_membership(&self, vg: NodeId) -> LiveCheckStatus {
        let unknown = || {
            LiveCheckStatus::Unknown { required_evidence: "complete, present PV membership and physical backing consistent with the measured VG count".into() }
        };
        if !self
            .capacity
            .iter()
            .any(|item| item.domain == AllocationDomain::LvmVolumeGroup(vg))
        {
            return unknown();
        }
        let counts: Vec<_> = self.vg_members.iter().filter(|(id, _)| *id == vg).collect();
        let [(_, expected)] = counts.as_slice() else {
            return unknown();
        };
        let graph = &self.observation.snapshot.graph;
        let members: Vec<_> = graph
            .dependencies()
            .filter(|edge| {
                edge.to == vg && edge.kind == DependencyKind::MemberOf(MemberRole::LvmPv)
            })
            .collect();
        if *expected == 0 || members.len() as u64 != *expected {
            return unknown();
        }
        for edge in members {
            let Some(node) = graph.node(&edge.from) else {
                return unknown();
            };
            if node.size.presence != Presence::Present
                || node.kind.kind != NodeKind::LvmPv
                || !node.kind.size.is_some_and(|size| size.as_u64() != 0)
            {
                return unknown();
            }
            let identities: Vec<_> = node
                .size
                .identities
                .iter()
                .filter(|id| matches!(id, ExternalId::LvmPvUuid(_)))
                .collect();
            let [ExternalId::LvmPvUuid(uuid)] = identities.as_slice() else {
                return unknown();
            };
            if uuid.trim().is_empty()
                || uuid.trim() != uuid
                || graph
                    .nodes()
                    .filter(|(_, node)| node.size.identities.contains(identities[0]))
                    .count()
                    != 1
                || graph
                    .dependencies()
                    .filter(|other| {
                        other.from == edge.from
                            && other.kind == DependencyKind::MemberOf(MemberRole::LvmPv)
                    })
                    .count()
                    != 1
                || !graph.ancestors(edge.from).iter().any(|id| {
                    graph.node(id).is_some_and(|node| {
                        matches!(node.kind.kind, NodeKind::Disk | NodeKind::NvmeNamespace(_))
                    })
                })
            {
                return unknown();
            }
        }
        LiveCheckStatus::Satisfied
    }

    /// Checks observed facts and allocation counts retained from this exact provider pass.
    pub fn check_prerequisites(
        &self,
        baseline: &HistoricalBaseline,
        prerequisites: &[ActionPrerequisite],
    ) -> Result<ObservedPrerequisites, ObservationError> {
        check_observed_prerequisites_with_capacity(
            baseline,
            &self.observation,
            &self.capacity,
            prerequisites,
        )
    }

    /// Checks the same facts with UUID correlations validated by this run's output bindings.
    pub(crate) fn check_prerequisites_with_bindings(
        &self,
        baseline: &HistoricalBaseline,
        prerequisites: &[ActionPrerequisite],
        bindings: &[storage_engine::editing::NodeCorrelation],
    ) -> Result<ObservedPrerequisites, ObservationError> {
        storage_engine::live::check_observed_prerequisites_with_bindings(
            baseline,
            &self.observation,
            &self.capacity,
            prerequisites,
            bindings,
        )
    }
}
