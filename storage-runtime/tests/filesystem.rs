//! Synthetic C4 evidence tests; no native filesystem operations are performed.

use std::error::Error;
use storage_core::model::{
    CurrentState, Dependency, DependencyKind, ExternalId, FilesystemKind, LvmLv, LvmLvKind, NodeId,
    NodeKind, ProbeEpoch,
};
use storage_engine::{LiveObservation, ObservationReference};
use storage_runtime::filesystem::correlate_created_ext4;
use uuid::Uuid;

/// Same-instance pre/post publications with a new ext4 on an existing UUID-qualified LV.
fn fixture() -> Result<(LiveObservation, LiveObservation, NodeId, NodeId, String), Box<dyn Error>> {
    let raw: serde_json::Value = serde_json::from_str(include_str!(
        "../../storage-engine/examples/fixtures/probe_snapshot.json"
    ))?;
    let mut snapshot: CurrentState = serde_json::from_value(raw["current"].clone())?;
    let mut lv = snapshot.graph.nodes().next().ok_or("node")?.1.clone();
    lv.kind.kind = NodeKind::LvmLv(LvmLv {
        name: "data".into(),
        kind: LvmLvKind::Linear,
    });
    lv.size.identities = vec![ExternalId::LvmLvUuid("created-lv".into())];
    let backing = NodeId::new();
    snapshot.graph.insert_new(backing, lv.clone())?;
    let before = LiveObservation {
        reference: ObservationReference {
            probe_instance: Uuid::now_v7(),
            epoch: ProbeEpoch::new(1),
        },
        snapshot,
    };
    let mut after = before.clone();
    after.reference.epoch = ProbeEpoch::new(2);
    let uuid = Uuid::now_v7().to_string();
    let actual = NodeId::new();
    lv.kind.kind = NodeKind::Filesystem {
        kind: FilesystemKind::Ext4,
        label: None,
    };
    lv.size.identities = vec![ExternalId::Filesystem {
        fs_type: "ext4".into(),
        value: uuid.clone(),
    }];
    after.snapshot.graph.insert_new(actual, lv)?;
    after.snapshot.graph.insert_dependency(Dependency {
        from: backing,
        to: actual,
        kind: DependencyKind::Backs,
    });
    Ok((before, after, backing, actual, uuid))
}

/// A valid UUID is insufficient without unique identity, exact properties and owning backing.
#[test]
fn ext4_correlation_checks_identity_properties_and_backing() -> Result<(), Box<dyn Error>> {
    let (before, after, backing, actual, uuid) = fixture()?;
    let planned = NodeId::new();
    assert_eq!(
        correlate_created_ext4(&before, &after, planned, backing, &uuid, &None)?.target,
        actual
    );
    for case in 0..7 {
        let mut changed = after.clone();
        match case {
            0 => changed.reference = before.reference,
            1 => changed.reference.probe_instance = Uuid::now_v7(),
            2 => {
                let node = changed.snapshot.graph.node(&actual).ok_or("FS")?.clone();
                changed.snapshot.graph.insert_new(NodeId::new(), node)?;
            }
            3 => {
                let mut node = changed.snapshot.graph.node(&actual).ok_or("FS")?.clone();
                node.kind.kind = NodeKind::Filesystem {
                    kind: FilesystemKind::Xfs,
                    label: None,
                };
                changed.snapshot.graph.replace_existing(actual, node)?;
            }
            4 => {
                let edge = changed
                    .snapshot
                    .graph
                    .dependencies()
                    .find(|edge| edge.to == actual)
                    .ok_or("edge")?
                    .clone();
                changed.snapshot.graph.remove_dependency(&edge);
            }
            5 => {
                let mut node = changed.snapshot.graph.node(&actual).ok_or("FS")?.clone();
                node.size.identities.push(node.size.identities[0].clone());
                changed.snapshot.graph.replace_existing(actual, node)?;
            }
            _ => {
                let mut node = changed.snapshot.graph.node(&actual).ok_or("FS")?.clone();
                node.size.presence = storage_core::model::Presence::Unknown;
                changed.snapshot.graph.replace_existing(actual, node)?;
            }
        }
        assert!(
            correlate_created_ext4(&before, &changed, planned, backing, &uuid, &None).is_err(),
            "case {case}"
        );
    }
    assert!(
        correlate_created_ext4(&before, &after, planned, backing, "not-a-uuid", &None).is_err()
    );
    assert!(
        correlate_created_ext4(
            &before,
            &after,
            planned,
            backing,
            &uuid,
            &Some("wrong".into())
        )
        .is_err()
    );
    Ok(())
}
