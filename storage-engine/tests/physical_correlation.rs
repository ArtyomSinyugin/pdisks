//! Stable physical LVM ancestry is correlated without using /dev paths or device numbers.

use std::{error::Error, num::NonZeroU32};
use storage_core::model::{
    Bytes, CurrentState, Dependency, DependencyKind, ExternalId, Node, NodeFacts, NodeGraph,
    NodeId, NodeKind, NodeSpec, PartitionAttributes, PartitionTable, Presence,
};
use storage_engine::editing::correlate_nodes;
use uuid::Uuid;

/// Builds one present node with one explicit durable identity.
fn node(kind: NodeKind, identity: ExternalId) -> Node {
    Node {
        kind: NodeSpec { kind, size: None },
        size: NodeFacts {
            identities: vec![identity],
            presence: Presence::Present,
            ..NodeFacts::default()
        },
    }
}

/// Uses the existing host fixture envelope with only the selected storage graph replaced.
fn state(graph: NodeGraph) -> Result<CurrentState, Box<dyn Error>> {
    let raw: serde_json::Value =
        serde_json::from_str(include_str!("../examples/fixtures/probe_snapshot.json"))?;
    let mut state: CurrentState = serde_json::from_value(raw["current"].clone())?;
    state.graph = graph;
    Ok(state)
}

/// PV UUID, disk WWN, and GPT partition UUID survive transient IDs and reject duplicates/replacements.
#[test]
fn physical_identities_are_unique_present_and_kind_compatible() -> Result<(), Box<dyn Error>> {
    let cases = [
        node(NodeKind::Disk, ExternalId::Wwn("wwn-test".into())),
        node(NodeKind::LvmPv, ExternalId::LvmPvUuid("pv-test".into())),
        node(
            NodeKind::Partition {
                number: NonZeroU32::new(1).ok_or("number")?,
                offset: Bytes::new(1024 * 1024),
                role: None,
                attributes: PartitionAttributes::default(),
            },
            ExternalId::GptPartitionUuid(Uuid::now_v7()),
        ),
    ];
    for original in cases {
        let source = NodeId::new();
        let target = NodeId::new();
        let mut graph = NodeGraph::new();
        graph.insert_new(source, original.clone())?;
        let current = state(graph)?;
        let mut graph = NodeGraph::new();
        graph.insert_new(target, original.clone())?;
        let next = state(graph)?;
        let mapped = correlate_nodes(&current, &next, &[source])?;
        assert_eq!(mapped[0].target, target);
        for case in 0..4 {
            let mut changed = next.clone();
            let mut value = original.clone();
            match case {
                0 => value.size.presence = Presence::Missing,
                1 => value.size.identities.clear(),
                2 => value.size.identities.push(value.size.identities[0].clone()),
                _ => {
                    changed.graph.insert_new(NodeId::new(), original.clone())?;
                }
            }
            changed.graph.replace_existing(target, value)?;
            assert!(
                correlate_nodes(&current, &changed, &[source]).is_err(),
                "case {case}"
            );
        }
    }
    let id = NodeId::new();
    let mut graph = NodeGraph::new();
    graph.insert_new(
        id,
        node(
            NodeKind::Disk,
            ExternalId::Serial {
                vendor: None,
                value: "serial-only".into(),
            },
        ),
    )?;
    let source = state(graph)?;
    assert!(correlate_nodes(&source, &source, &[id]).is_err());
    Ok(())
}

/// Tables use exact backing-disk identity and reject another disk, multiple tables, or shared backing.
#[test]
fn partition_table_requires_one_uniquely_identified_backing_disk() -> Result<(), Box<dyn Error>> {
    let disk = NodeId::new();
    let table = NodeId::new();
    let disk_node = node(NodeKind::Disk, ExternalId::Wwn("disk-wwn".into()));
    let mut table_node = node(
        NodeKind::PartitionTable(PartitionTable::Gpt),
        ExternalId::Wwn("not-used".into()),
    );
    table_node.size.identities.clear();
    let mut graph = NodeGraph::new();
    graph.insert_new(disk, disk_node.clone())?;
    graph.insert_new(table, table_node.clone())?;
    graph.insert_dependency(Dependency {
        from: disk,
        to: table,
        kind: DependencyKind::Backs,
    });
    let current = state(graph)?;
    let new_disk = NodeId::new();
    let new_table = NodeId::new();
    let mut graph = NodeGraph::new();
    graph.insert_new(new_disk, disk_node.clone())?;
    graph.insert_new(new_table, table_node.clone())?;
    graph.insert_dependency(Dependency {
        from: new_disk,
        to: new_table,
        kind: DependencyKind::Backs,
    });
    let next = state(graph)?;
    let mapped = correlate_nodes(&current, &next, &[disk, table])?;
    assert_eq!(mapped[0].target, new_disk);
    assert_eq!(mapped[1].target, new_table);
    assert_eq!(mapped[1].identity, ExternalId::Wwn("disk-wwn".into()));
    for case in 0..4 {
        let mut changed = next.clone();
        match case {
            0 => {
                let mut replacement = disk_node.clone();
                replacement.size.identities = vec![ExternalId::Wwn("other-disk".into())];
                changed.graph.replace_existing(new_disk, replacement)?;
            }
            1 => {
                let id = NodeId::new();
                changed.graph.insert_new(id, table_node.clone())?;
                changed.graph.insert_dependency(Dependency {
                    from: new_disk,
                    to: id,
                    kind: DependencyKind::Backs,
                });
            }
            2 => {
                let id = NodeId::new();
                changed
                    .graph
                    .insert_new(id, node(NodeKind::Disk, ExternalId::Wwn("other".into())))?;
                changed.graph.insert_dependency(Dependency {
                    from: id,
                    to: new_table,
                    kind: DependencyKind::Backs,
                });
            }
            _ => {
                let mut wrong = table_node.clone();
                wrong.kind.kind = NodeKind::PartitionTable(PartitionTable::Mbr);
                changed.graph.replace_existing(new_table, wrong)?;
            }
        }
        assert!(
            correlate_nodes(&current, &changed, &[table]).is_err(),
            "case {case}"
        );
    }
    Ok(())
}

/// NVMe namespace UUID/NGUID/EUI are accepted only when nonzero and unambiguous.
#[test]
fn nvme_namespace_identity_rejects_zero_and_conflicting_records() -> Result<(), Box<dyn Error>> {
    use storage_core::model::{BlockSize, NvmeLbaFormat, NvmeNamespace, NvmeNamespaceId};
    let kind = NodeKind::NvmeNamespace(NvmeNamespace {
        nsid: NvmeNamespaceId::new(1).ok_or("nsid")?,
        format: NvmeLbaFormat {
            data_size: BlockSize::new(4096).ok_or("block")?,
            metadata_size: 0,
        },
    });
    for identity in [
        ExternalId::NvmeUuid(Uuid::now_v7()),
        ExternalId::NvmeNguid([1; 16]),
        ExternalId::NvmeEui64([2; 8]),
    ] {
        let id = NodeId::new();
        let mut graph = NodeGraph::new();
        graph.insert_new(id, node(kind.clone(), identity))?;
        let current = state(graph)?;
        assert_eq!(correlate_nodes(&current, &current, &[id])?[0].target, id);
        let mut changed = current.clone();
        let mut value = current.graph.node(&id).ok_or("namespace")?.clone();
        value.size.identities.push(ExternalId::NvmeNguid([3; 16]));
        changed.graph.replace_existing(id, value)?;
        assert!(correlate_nodes(&changed, &changed, &[id]).is_err());
    }
    for identity in [
        ExternalId::NvmeUuid(Uuid::nil()),
        ExternalId::NvmeNguid([0; 16]),
        ExternalId::NvmeEui64([0; 8]),
    ] {
        let id = NodeId::new();
        let mut graph = NodeGraph::new();
        graph.insert_new(id, node(kind.clone(), identity))?;
        let current = state(graph)?;
        assert!(correlate_nodes(&current, &current, &[id]).is_err());
    }
    Ok(())
}
