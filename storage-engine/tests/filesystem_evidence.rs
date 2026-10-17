//! Conditional filesystem evidence from actual UUID-qualified observations; no storage writes.

use std::error::Error;
use storage_core::model::{
    Bytes, CurrentState, Dependency, DependencyKind, ExternalId, FilesystemKind, LvmLv, LvmLvKind,
    Node, NodeFacts, NodeId, NodeKind, NodeSpec, Presence, ProbeEpoch,
};
use storage_engine::{
    HistoricalBaseline, LiveCheckStatus, LiveObservation, ObservationReference,
    editing::NodeCorrelation, live::check_observed_postconditions_with_bindings,
};
use storage_provider::Postcondition;
use uuid::Uuid;

/// Existing LV plus a newly observed filesystem, with distinct planned and actual node identities.
struct Fixture {
    /// Reviewed state before filesystem creation.
    baseline: HistoricalBaseline,
    /// Actual fresh result, independent of transient planned IDs.
    observed: LiveObservation,
    /// Stable backing LV already present in the baseline.
    backing: NodeId,
    /// UUID-qualified provisional mapping; runtime must still prove ownership and creation provenance.
    binding: NodeCorrelation,
    /// Requested filesystem properties checked independently of the UUID mapping.
    spec: NodeSpec,
}

impl Fixture {
    /// Creates an explicitly synthetic ext4 observation on a UUID-qualified linear LV.
    fn new() -> Result<Self, Box<dyn Error>> {
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../examples/fixtures/probe_snapshot.json"))?;
        let mut snapshot: CurrentState = serde_json::from_value(raw["current"].clone())?;
        let backing = NodeId::new();
        snapshot.graph.insert_new(
            backing,
            Node {
                kind: NodeSpec {
                    kind: NodeKind::LvmLv(LvmLv {
                        name: "data".into(),
                        kind: LvmLvKind::Linear,
                    }),
                    size: Some(Bytes::new(16 * 1024 * 1024)),
                },
                size: NodeFacts {
                    presence: Presence::Present,
                    identities: vec![ExternalId::LvmLvUuid("lv-uuid".into())],
                    ..NodeFacts::default()
                },
            },
        )?;
        let baseline = HistoricalBaseline {
            id: Uuid::now_v7(),
            snapshot,
            capacity_evidence: vec![],
        };
        let planned = NodeId::new();
        let actual = NodeId::new();
        let identity = ExternalId::Filesystem {
            fs_type: "ext4".into(),
            value: "fs-uuid".into(),
        };
        let spec = NodeSpec {
            kind: NodeKind::Filesystem {
                kind: FilesystemKind::Ext4,
                label: Some("data".into()),
            },
            size: Some(Bytes::new(16 * 1024 * 1024)),
        };
        let mut observed = LiveObservation {
            reference: ObservationReference {
                probe_instance: Uuid::now_v7(),
                epoch: ProbeEpoch::new(1),
            },
            snapshot: baseline.snapshot.clone(),
        };
        observed.snapshot.graph.insert_new(
            actual,
            Node {
                kind: spec.clone(),
                size: NodeFacts {
                    presence: Presence::Present,
                    identities: vec![identity.clone()],
                    ..NodeFacts::default()
                },
            },
        )?;
        observed.snapshot.graph.insert_dependency(Dependency {
            from: backing,
            to: actual,
            kind: DependencyKind::Backs,
        });
        Ok(Self {
            baseline,
            observed,
            backing,
            binding: NodeCorrelation {
                source: planned,
                target: actual,
                identity,
            },
            spec,
        })
    }
}

/// UUID evidence cannot bypass properties, backing dependency, uniqueness, type or actual presence.
#[test]
fn filesystem_output_checks_require_unique_typed_uuid_and_actual_properties()
-> Result<(), Box<dyn Error>> {
    let f = Fixture::new()?;
    let conditions = [
        Postcondition::NodeMatches {
            id: f.binding.source,
            spec: f.spec.clone(),
        },
        Postcondition::FilesystemLabel {
            filesystem: f.binding.source,
            label: Some("data".into()),
        },
        Postcondition::DependencyPresent(Dependency {
            from: f.backing,
            to: f.binding.source,
            kind: DependencyKind::Backs,
        }),
    ];
    let report = check_observed_postconditions_with_bindings(
        &f.baseline,
        &f.observed,
        &conditions,
        std::slice::from_ref(&f.binding),
    )?;
    assert!(
        report
            .checks
            .iter()
            .all(|check| check.status == LiveCheckStatus::Satisfied)
    );
    for case in 0..8 {
        let mut observed = f.observed.clone();
        let mut node = observed
            .snapshot
            .graph
            .node(&f.binding.target)
            .ok_or("FS")?
            .clone();
        let mut bindings = vec![f.binding.clone()];
        match case {
            0 => bindings.clear(),
            1 => {
                observed
                    .snapshot
                    .graph
                    .insert_new(NodeId::new(), node.clone())?;
            }
            2 => bindings.push(f.binding.clone()),
            3 => bindings[0].identity = ExternalId::LvmLvUuid("fs-uuid".into()),
            4 => node.size.identities.clear(),
            5 => {
                node.kind.kind = NodeKind::Filesystem {
                    kind: FilesystemKind::Xfs,
                    label: None,
                }
            }
            6 => node.size.identities.push(node.size.identities[0].clone()),
            _ => node.size.presence = Presence::Unknown,
        }
        observed
            .snapshot
            .graph
            .replace_existing(f.binding.target, node)?;
        let report = check_observed_postconditions_with_bindings(
            &f.baseline,
            &observed,
            &conditions,
            &bindings,
        )?;
        assert!(
            report
                .checks
                .iter()
                .all(|check| matches!(check.status, LiveCheckStatus::Unknown { .. })),
            "case {case}"
        );
    }
    let mut wrong = f.observed.clone();
    let mut node = wrong
        .snapshot
        .graph
        .node(&f.binding.target)
        .ok_or("FS")?
        .clone();
    node.kind.size = Some(Bytes::new(4096));
    wrong
        .snapshot
        .graph
        .replace_existing(f.binding.target, node)?;
    assert!(matches!(
        check_observed_postconditions_with_bindings(
            &f.baseline,
            &wrong,
            &conditions,
            &[f.binding]
        )?
        .checks[0]
            .status,
        LiveCheckStatus::Violated { .. }
    ));
    Ok(())
}

/// Mount evidence must be unique at the target; a matching older row cannot hide an overmount.
#[test]
fn mount_postcondition_rejects_ambiguous_and_unresolved_sources() -> Result<(), Box<dyn Error>> {
    use storage_core::model::{MountContext, MountSource, ObservedMount};
    let f = Fixture::new()?;
    let condition = Postcondition::RuntimeMountPresent {
        source: MountSource::Filesystem(f.binding.source),
        target: "/data".into(),
        context: MountContext::Host,
    };
    let mount = ObservedMount {
        source: MountSource::Filesystem(f.binding.target),
        target: "/data".into(),
        options: vec!["rw".into()],
        context: MountContext::Host,
    };
    let mut observed = f.observed.clone();
    observed.snapshot.mounts.entries.push(mount.clone());
    assert_eq!(
        check_observed_postconditions_with_bindings(
            &f.baseline,
            &observed,
            std::slice::from_ref(&condition),
            std::slice::from_ref(&f.binding)
        )?
        .checks[0]
            .status,
        LiveCheckStatus::Satisfied
    );
    for case in 0..6 {
        let mut changed = observed.clone();
        match case {
            0 => changed.snapshot.mounts.entries.clear(),
            1 => changed.snapshot.mounts.entries.push(mount.clone()),
            2 => changed.snapshot.mounts.entries[0].source = MountSource::Tmpfs,
            3 => changed.snapshot.mounts.entries[0].source = MountSource::Other("/dev/dm-1".into()),
            4 => {
                changed.snapshot.mounts.entries[0].context = MountContext::Namespace("other".into())
            }
            _ => {
                let mut overmount = mount.clone();
                overmount.source = MountSource::Tmpfs;
                changed.snapshot.mounts.entries.push(overmount);
            }
        }
        assert_ne!(
            check_observed_postconditions_with_bindings(
                &f.baseline,
                &changed,
                std::slice::from_ref(&condition),
                std::slice::from_ref(&f.binding)
            )?
            .checks[0]
                .status,
            LiveCheckStatus::Satisfied,
            "case {case}"
        );
    }
    let namespaced = Postcondition::RuntimeMountPresent {
        source: MountSource::Filesystem(f.binding.source),
        target: "/data".into(),
        context: MountContext::Namespace("other".into()),
    };
    assert!(matches!(
        check_observed_postconditions_with_bindings(
            &f.baseline,
            &observed,
            &[namespaced],
            &[f.binding]
        )?
        .checks[0]
            .status,
        LiveCheckStatus::Unknown { .. }
    ));
    Ok(())
}

/// Final mount goals compare durable source and explicit options, not a backend success flag.
#[test]
fn final_mount_goal_checks_options_and_preserves_unknown_evidence() -> Result<(), Box<dyn Error>> {
    use storage_core::model::{MountContext, MountSource, ObservedMount};
    use storage_engine::{
        ExecutionRun, GoalAspect, editing::PlanSession, live::check_observed_goal,
    };
    let f = Fixture::new()?;
    // The filesystem is already present for this read-only goal; no filesystem action is authorized.
    let mut baseline = f.baseline.clone();
    baseline.snapshot = f.observed.snapshot.clone();
    let mount = ObservedMount {
        source: MountSource::Filesystem(f.binding.target),
        target: "/data".into(),
        options: vec!["rw".into(), "nodev".into()],
        context: MountContext::Host,
    };
    baseline.snapshot.mounts.entries.push(mount.clone());
    let session = PlanSession::new(baseline)?;
    let frozen = session.freeze(session.revision())?;
    let run = ExecutionRun::new(frozen.document())?;
    let mut observed = f.observed.clone();
    observed.snapshot.mounts.entries.push(mount);
    for (options, expected) in [
        (vec!["nodev", "rw"], "satisfied"),
        (vec!["nodev", "ro"], "violated"),
        (vec!["rw"], "violated"),
        (vec!["rw", "ro", "nodev"], "unknown"),
        (vec!["rw", "nodev", "nodev"], "unknown"),
        (vec!["defaults"], "unknown"),
        (vec![], "unknown"),
    ] {
        observed.snapshot.mounts.entries[0].options =
            options.into_iter().map(str::to_owned).collect();
        let checks = check_observed_goal(&run.context(), &observed, &[])?;
        let status = &checks
            .iter()
            .find(|check| matches!(check.condition, GoalAspect::RuntimeMount { .. }))
            .ok_or("mount check")?
            .status;
        match expected {
            "satisfied" => assert_eq!(*status, LiveCheckStatus::Satisfied),
            "violated" => assert!(matches!(status, LiveCheckStatus::Violated { .. })),
            _ => assert!(matches!(status, LiveCheckStatus::Unknown { .. })),
        }
    }
    Ok(())
}
