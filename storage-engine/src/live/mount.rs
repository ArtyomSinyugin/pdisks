//! Strict observed host mount matching shared by action and final goal verification.

use super::{unknown, verify::result_node};
use crate::{HistoricalBaseline, LiveCheckStatus, LiveObservation, editing::NodeCorrelation};
use std::path::{Component, Path};
use storage_core::model::{MountContext, MountSource, NodeKind, ObservedMount};

/// Finds exactly one observed host mount and correlates its source by durable identity.
/// Missing/ambiguous records or unresolved sources do not prove either success or absence.
pub(super) fn observed_mount<'a>(
    baseline: &HistoricalBaseline,
    observed: &'a LiveObservation,
    source: &MountSource,
    target: &Path,
    context: &MountContext,
    bindings: &[NodeCorrelation],
) -> Result<&'a ObservedMount, LiveCheckStatus> {
    if *context != MountContext::Host {
        return Err(unknown("independent mount namespace evidence is required"));
    }
    if !target.is_absolute()
        || target.components().any(|part| part == Component::ParentDir)
        || target.as_os_str().as_encoded_bytes().contains(&0)
    {
        return Err(unknown("mount target must be a normalized absolute path"));
    }
    let source = match source {
        MountSource::Filesystem(id) => {
            let (id, node) = result_node(baseline, observed, *id, bindings)?;
            if !matches!(node.kind.kind, NodeKind::Filesystem { .. }) {
                return Err(LiveCheckStatus::Violated {
                    reason: "correlated mount source is not a filesystem".into(),
                });
            }
            MountSource::Filesystem(id)
        }
        MountSource::Tmpfs => MountSource::Tmpfs,
        _ => {
            return Err(unknown(
                "independent evidence for this mount source is required",
            ));
        }
    };
    let mut matches = observed
        .snapshot
        .mounts
        .entries
        .iter()
        .filter(|mount| mount.target == target && mount.context == *context);
    let mount = matches.next().ok_or_else(|| {
        unknown("requested mount was not observed; complete coverage is required")
    })?;
    if matches.next().is_some() {
        return Err(unknown(
            "multiple mount records occupy the requested target",
        ));
    }
    if matches!(mount.source, MountSource::Other(_)) {
        return Err(unknown("observed mount source is unresolved"));
    }
    if mount.source != source {
        return Err(LiveCheckStatus::Violated {
            reason: "mount target is occupied by a different source".into(),
        });
    }
    Ok(mount)
}
