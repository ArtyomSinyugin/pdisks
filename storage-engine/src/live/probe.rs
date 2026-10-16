//! Same-pass probe publications and conservative live LVM capacity checks.

use storage_core::model::{NodeKind, ProbeEpoch};
use storage_probe::LinuxProbe;
use storage_provider::{ActionPrerequisite, AllocationDomain, AllocationQuantity};
use thiserror::Error;

use super::{
    ObservationError, ObservedPrerequisites, check_observed_prerequisites, correlated_node,
    qualify_observed_status, unknown,
};
use crate::{
    CapacityEvidence, HistoricalBaseline, LiveCheckStatus, LiveObservation, ObservationReference,
    PlanSimulationError,
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
}

impl ProbedObservation {
    /// Calls the selected probe once and binds graph/capacity to its instance and generation.
    /// No predicted graph or historical free-space values enter this publication.
    pub fn collect(probe: &LinuxProbe<'_>) -> Result<Self, ProbeObservationError> {
        let publication = probe.probe();
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
        })
    }

    /// Borrows the immutable publication used by every check on this value.
    pub fn observation(&self) -> &LiveObservation {
        &self.observation
    }

    /// Transfers actual state to the runtime after all checks have used this same publication.
    pub fn into_observation(self) -> LiveObservation {
        self.observation
    }

    /// Adds same-pass capacity checks to the conservative observed-fact checks.
    /// Frozen allocation units must still match current VG geometry; other facts stay unknown.
    pub fn check_prerequisites(
        &self,
        baseline: &HistoricalBaseline,
        prerequisites: &[ActionPrerequisite],
    ) -> Result<ObservedPrerequisites, ObservationError> {
        let mut report = check_observed_prerequisites(baseline, &self.observation, prerequisites)?;
        for check in &mut report.checks {
            if let ActionPrerequisite::CapacityAvailable { domain, amount } = &check.condition {
                check.status = qualify_observed_status(
                    &self.observation,
                    self.check_capacity(baseline, domain, *amount),
                );
            }
        }
        Ok(report)
    }

    /// Correlates the measured VG and refuses to reinterpret frozen counts in changed extent units.
    fn check_capacity(
        &self,
        baseline: &HistoricalBaseline,
        domain: &AllocationDomain,
        amount: Option<AllocationQuantity>,
    ) -> LiveCheckStatus {
        let Some(amount) = amount else {
            return unknown("exact provider allocation requirement is needed");
        };
        let AllocationDomain::LvmVolumeGroup(source) = domain;
        let (target, current) = match correlated_node(baseline, &self.observation, *source) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let original = baseline.snapshot.graph.node(source);
        if !matches!((original.map(|node| &node.kind.kind), &current.kind.kind),
            (Some(NodeKind::LvmVg { extent_size: Some(old), .. }), NodeKind::LvmVg { extent_size: Some(new), .. }) if old == new)
        {
            return unknown(
                "VG allocation units changed or are unknown; review the frozen plan again",
            );
        }
        let Some(measured) = self
            .capacity
            .iter()
            .find(|value| value.domain == AllocationDomain::LvmVolumeGroup(target))
        else {
            return unknown("validated same-pass VG free capacity is required");
        };
        if measured.available >= amount {
            LiveCheckStatus::Satisfied
        } else {
            LiveCheckStatus::Violated {
                reason: "fresh VG free capacity is smaller than the frozen allocation requirement"
                    .into(),
            }
        }
    }
}
