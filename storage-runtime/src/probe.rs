//! Same-pass probe publications and conservative live LVM capacity checks.

use storage_core::model::ProbeEpoch;
use storage_probe::LinuxProbe;
use storage_provider::ActionPrerequisite;
use thiserror::Error;

use storage_engine::{
    CapacityEvidence, HistoricalBaseline, LiveObservation, ObservationReference,
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
}
