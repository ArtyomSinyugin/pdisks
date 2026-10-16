//! Pure same-publication capacity checks; collection belongs to storage-runtime.

use super::{
    ObservationError, ObservedPrerequisites, check_prerequisites_with_bindings, correlated_node,
    qualify_observed_status, unknown,
};
use crate::{CapacityEvidence, HistoricalBaseline, LiveCheckStatus, LiveObservation};
use storage_core::model::NodeKind;
use storage_provider::{ActionPrerequisite, AllocationDomain, AllocationQuantity};

/// Adds measured capacity checks without reading the host or reusing historical free-space counts.
/// The trusted host must supply capacity from the exact provider pass producing this observation.
pub fn check_observed_prerequisites_with_capacity(
    baseline: &HistoricalBaseline,
    observation: &LiveObservation,
    capacity: &[CapacityEvidence],
    prerequisites: &[ActionPrerequisite],
) -> Result<ObservedPrerequisites, ObservationError> {
    check_observed_prerequisites_with_bindings(baseline, observation, capacity, prerequisites, &[])
}

/// Checks actual capacity and UUID-qualified planned outputs without trusting predicted nodes.
/// The runtime must supply only bindings already verified in this execution attempt.
/// Existing baseline identities cannot be overridden by an output binding.
pub fn check_observed_prerequisites_with_bindings(
    baseline: &HistoricalBaseline,
    observation: &LiveObservation,
    capacity: &[CapacityEvidence],
    prerequisites: &[ActionPrerequisite],
    bindings: &[crate::editing::NodeCorrelation],
) -> Result<ObservedPrerequisites, ObservationError> {
    let mut report =
        check_prerequisites_with_bindings(baseline, observation, prerequisites, bindings)?;
    for check in &mut report.checks {
        if let ActionPrerequisite::CapacityAvailable { domain, amount } = &check.condition {
            check.status = qualify_observed_status(
                observation,
                check_capacity(baseline, observation, capacity, domain, *amount),
            );
        }
    }
    Ok(report)
}

/// Correlates the measured VG and refuses to reinterpret frozen counts in changed extent units.
fn check_capacity(
    baseline: &HistoricalBaseline,
    observation: &LiveObservation,
    capacity: &[CapacityEvidence],
    domain: &AllocationDomain,
    amount: Option<AllocationQuantity>,
) -> LiveCheckStatus {
    let Some(amount) = amount else {
        return unknown("exact provider allocation requirement is needed");
    };
    let AllocationDomain::LvmVolumeGroup(source) = domain;
    let (target, current) = match correlated_node(baseline, observation, *source) {
        Ok(value) => value,
        Err(status) => return status,
    };
    let original = baseline.snapshot.graph.node(source);
    if !matches!((original.map(|node| &node.kind.kind), &current.kind.kind),
            (Some(NodeKind::LvmVg { extent_size: Some(old), .. }), NodeKind::LvmVg { extent_size: Some(new), .. }) if old == new)
    {
        return unknown("VG allocation units changed or are unknown; review the frozen plan again");
    }
    let Some(measured) = capacity
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
