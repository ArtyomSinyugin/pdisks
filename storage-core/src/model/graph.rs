//! Storage graph and relationship semantics.

use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::Entry},
    num::NonZeroU64,
};

use serde::{Deserialize, Serialize};

use super::{Bytes, Node, NodeId, NodeKind, ZfsMember};
use thiserror::Error;

/// Canonical collection of storage nodes and their relationships.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "NodeGraphDto")]
pub struct NodeGraph {
    nodes: HashMap<NodeId, Node>,
    dependencies: HashSet<Dependency>,
    relations: HashSet<Relation>,
}

impl NodeGraph {
    /// Creates an empty storage graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a new identity without overwriting an existing observation or spec.
    pub fn insert_new(&mut self, id: NodeId, node: Node) -> Result<(), GraphError> {
        match self.nodes.entry(id) {
            Entry::Vacant(entry) => {
                entry.insert(node);
                Ok(())
            }
            Entry::Occupied(_) => Err(GraphError::DuplicateNode(id)),
        }
    }

    /// Replaces an existing identity without accidentally creating a new object.
    pub fn replace_existing(&mut self, id: NodeId, node: Node) -> Result<Node, GraphError> {
        let current = self.nodes.get_mut(&id).ok_or(GraphError::MissingNode(id))?;
        Ok(std::mem::replace(current, node))
    }

    /// Adds a dependency, returning whether it was new.
    pub fn insert_dependency(&mut self, dependency: Dependency) -> bool {
        self.dependencies.insert(dependency)
    }

    /// Adds a relation, returning whether it was new.
    pub fn insert_relation(&mut self, relation: Relation) -> bool {
        self.relations.insert(relation)
    }

    /// Removes a node, returning its last published value when present.
    ///
    /// Incident edges are intentionally left untouched so a caller can apply
    /// one graph-effect batch in its declared or validated order.
    pub fn remove_node(&mut self, id: &NodeId) -> Option<Node> {
        self.nodes.remove(id)
    }

    /// Removes an exact dependency edge, returning whether it was present.
    pub fn remove_dependency(&mut self, dependency: &Dependency) -> bool {
        self.dependencies.remove(dependency)
    }

    /// Removes an exact non-owning relation, returning whether it was present.
    pub fn remove_relation(&mut self, relation: &Relation) -> bool {
        self.relations.remove(relation)
    }

    /// Returns a node by canonical identity.
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// Iterates over all canonical nodes.
    pub fn nodes(&self) -> impl ExactSizeIterator<Item = (&NodeId, &Node)> {
        self.nodes.iter()
    }

    /// Iterates over all dependency edges.
    pub fn dependencies(&self) -> impl ExactSizeIterator<Item = &Dependency> {
        self.dependencies.iter()
    }

    /// Iterates over all non-owning relations.
    pub fn relations(&self) -> impl ExactSizeIterator<Item = &Relation> {
        self.relations.iter()
    }

    /// Visits dependency ancestors once; non-owning relations do not participate.
    pub fn ancestors(&self, id: NodeId) -> HashSet<NodeId> {
        let mut visited = HashSet::from([id]);
        let mut pending = vec![id];
        while let Some(node) = pending.pop() {
            for edge in self.dependencies.iter().filter(|edge| edge.to == node) {
                if visited.insert(edge.from) {
                    pending.push(edge.from);
                }
            }
        }
        visited.remove(&id);
        visited
    }

    /// Checks representation invariants without requiring healthy or complete storage.
    /// Missing nodes must be explicit placeholders; overlapping extents remain representable.
    /// Contains owners, MD data slots, Btrfs devids, and each cache role are unique per target.
    /// Multiple backing providers and unknown member slots remain valid.
    pub fn validate_structure(&self) -> Result<(), GraphError> {
        for edge in &self.relations {
            self.require_endpoints(edge.from, edge.to)?;
        }
        let mut owners = HashSet::new();
        let mut logical_edges = HashSet::new();
        let mut md_slots = HashSet::new();
        let mut btrfs_ids = HashSet::new();
        let mut cache_roles = HashSet::new();
        for edge in &self.dependencies {
            self.require_endpoints(edge.from, edge.to)?;
            if edge.from == edge.to {
                return Err(GraphError::SelfDependency(edge.from));
            }
            if !logical_edges.insert((
                edge.from,
                edge.to,
                std::mem::discriminant(&edge.kind),
                match &edge.kind {
                    DependencyKind::Caches(role) => Some(role),
                    _ => None,
                },
            )) {
                return Err(GraphError::ConflictingRole(edge.to));
            }
            match &edge.kind {
                DependencyKind::Contains { offset } => {
                    if !owners.insert(edge.to) {
                        return Err(GraphError::ConflictingOwner(edge.to));
                    }
                    if let Some(NodeKind::Partition {
                        offset: partition_offset,
                        ..
                    }) = self.node(&edge.to).map(|node| &node.kind.kind)
                        && offset != partition_offset
                    {
                        return Err(GraphError::PartitionOffset(edge.to));
                    }
                }
                DependencyKind::MemberOf(MemberRole::Md(member)) => {
                    if let Some(slot) = member.slot
                        && member.role == MdMemberRole::Data
                        && !md_slots.insert((edge.to, slot))
                    {
                        return Err(GraphError::ConflictingRole(edge.to));
                    }
                }
                DependencyKind::MemberOf(MemberRole::Btrfs(member)) => {
                    if let Some(devid) = member.devid
                        && !btrfs_ids.insert((edge.to, devid))
                    {
                        return Err(GraphError::ConflictingRole(edge.to));
                    }
                }
                DependencyKind::Caches(role) if !cache_roles.insert((edge.to, role)) => {
                    return Err(GraphError::ConflictingRole(edge.to));
                }
                _ => {}
            }
        }
        self.topological_order().map(|_| ())
    }

    /// Orders backing objects before consumers, rejecting dangling edges and cycles.
    /// Relations can contain cycles and never constrain dependency ordering.
    pub fn topological_order(&self) -> Result<Vec<NodeId>, GraphError> {
        let mut indegrees: HashMap<_, usize> = self.nodes.keys().map(|id| (*id, 0)).collect();
        let mut outgoing: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for edge in &self.dependencies {
            self.require_endpoints(edge.from, edge.to)?;
            if edge.from == edge.to {
                return Err(GraphError::SelfDependency(edge.from));
            }
            *indegrees.entry(edge.to).or_default() += 1;
            outgoing.entry(edge.from).or_default().push(edge.to);
        }
        let mut ready: VecDeque<_> = indegrees
            .iter()
            .filter_map(|(id, degree)| (*degree == 0).then_some(*id))
            .collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(id) = ready.pop_front() {
            order.push(id);
            for to in outgoing.get(&id).into_iter().flatten() {
                if let Some(degree) = indegrees.get_mut(to) {
                    *degree -= 1;
                    if *degree == 0 {
                        ready.push_back(*to);
                    }
                }
            }
        }
        if order.len() != self.nodes.len() {
            return Err(GraphError::DependencyCycle);
        }
        Ok(order)
    }

    /// Checks both endpoints before interpreting an edge's semantics.
    fn require_endpoints(&self, from: NodeId, to: NodeId) -> Result<(), GraphError> {
        for id in [from, to] {
            if !self.nodes.contains_key(&id) {
                return Err(GraphError::MissingNode(id));
            }
        }
        Ok(())
    }
}

/// Invalid graph representation or an invalid explicit node mutation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GraphError {
    /// Insertion would overwrite an existing node.
    #[error("node {0:?} already exists")]
    DuplicateNode(NodeId),
    /// An edge or replacement references an absent identity.
    #[error("node {0:?} is absent")]
    MissingNode(NodeId),
    /// Wire input repeats an exact dependency or relation.
    #[error("duplicate graph edge")]
    DuplicateEdge,
    /// A dependency references the same object twice.
    #[error("node {0:?} depends on itself")]
    SelfDependency(NodeId),
    /// Backing dependencies cannot be ordered.
    #[error("storage dependencies contain a cycle")]
    DependencyCycle,
    /// An object has multiple incompatible containing edges.
    #[error("node {0:?} has conflicting containing edges")]
    ConflictingOwner(NodeId),
    /// Two edges assign incompatible or repeated semantic roles.
    #[error("node {0:?} has conflicting dependency roles")]
    ConflictingRole(NodeId),
    /// Partition and containing edge disagree about the start position.
    #[error("partition {0:?} disagrees with its containing offset")]
    PartitionOffset(NodeId),
}

/// Untrusted wire fields preserve arrays until duplicates have been checked.
#[derive(Deserialize)]
struct NodeGraphDto {
    /// The existing JSON object format, read without silently overwriting keys.
    #[serde(deserialize_with = "deserialize_nodes")]
    nodes: HashMap<NodeId, Node>,
    /// Dependency duplicates must be detected before collecting into a set.
    dependencies: Vec<Dependency>,
    /// Relation duplicates must be detected before collecting into a set.
    relations: Vec<Relation>,
}

impl TryFrom<NodeGraphDto> for NodeGraph {
    type Error = GraphError;

    /// Publishes a graph only after wire duplicates and structural checks pass.
    fn try_from(dto: NodeGraphDto) -> Result<Self, Self::Error> {
        let mut graph = Self {
            nodes: dto.nodes,
            ..Self::new()
        };
        for edge in dto.dependencies {
            if !graph.insert_dependency(edge) {
                return Err(GraphError::DuplicateEdge);
            }
        }
        for edge in dto.relations {
            if !graph.insert_relation(edge) {
                return Err(GraphError::DuplicateEdge);
            }
        }
        graph.validate_structure()?;
        Ok(graph)
    }
}

/// Reads node entries directly so even repeated UUID object keys are rejected.
fn deserialize_nodes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<HashMap<NodeId, Node>, D::Error> {
    /// Map visitor retaining duplicate-key evidence before collection insertion.
    struct NodesVisitor;
    impl<'de> serde::de::Visitor<'de> for NodesVisitor {
        type Value = HashMap<NodeId, Node>;

        /// Describes the compatible graph node object expected on the wire.
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an object with unique node UUID keys")
        }

        /// Rejects repeated parsed identities, including equivalent UUID spellings.
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut nodes = HashMap::new();
            while let Some((id, node)) = map.next_entry::<NodeId, Node>()? {
                match nodes.entry(id) {
                    Entry::Vacant(entry) => {
                        entry.insert(node);
                    }
                    Entry::Occupied(_) => {
                        return Err(serde::de::Error::custom(GraphError::DuplicateNode(id)));
                    }
                }
            }
            Ok(nodes)
        }
    }
    deserializer.deserialize_map(NodesVisitor)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Dependency {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: DependencyKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Relation {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: RelationKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
pub struct BtrfsMember {
    pub devid: Option<NonZeroU64>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
pub struct MdMember {
    pub role: MdMemberRole,
    pub slot: Option<u16>,
    pub data_offset: Option<Bytes>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
pub enum MdMemberRole {
    Data,
    Spare,
    Replacement,
    Journal,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
pub enum MemberRole {
    LvmPv,
    Md(MdMember),
    Btrfs(BtrfsMember),
    Zfs(ZfsMember),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
pub enum CacheRole {
    Data,
    Metadata,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
pub enum DependencyKind {
    Backs,
    Contains { offset: Bytes },
    Provides,
    MemberOf(MemberRole),
    Caches(CacheRole),
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Hash, Serialize, Deserialize)]
pub enum RelationKind {
    SnapshotOf,
    NvmeAttachedTo,
    Replicates,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeFacts, NodeKind, NodeSpec};

    /// Builds a minimal node for graph mutation tests.
    fn disk_node() -> Node {
        Node {
            kind: NodeSpec {
                kind: NodeKind::Disk,
                size: None,
            },
            size: NodeFacts::default(),
        }
    }

    /// Removing a node does not silently rewrite separately modelled edges.
    #[test]
    fn remove_node_leaves_edges_for_the_effect_batch() {
        let from = NodeId::new();
        let to = NodeId::new();
        let dependency = Dependency {
            from,
            to,
            kind: DependencyKind::Backs,
        };
        let relation = Relation {
            from,
            to,
            kind: RelationKind::SnapshotOf,
        };
        let mut graph = NodeGraph::new();
        graph
            .insert_new(from, disk_node())
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        graph
            .insert_new(to, disk_node())
            .unwrap_or_else(|error| panic!("insert fixture node: {error}"));
        graph.insert_dependency(dependency.clone());
        graph.insert_relation(relation.clone());

        assert_eq!(graph.remove_node(&from), Some(disk_node()));
        assert!(graph.node(&from).is_none());
        assert!(graph.node(&to).is_some());
        assert!(graph.dependencies().any(|edge| edge == &dependency));
        assert!(graph.relations().any(|edge| edge == &relation));
    }

    /// Edge removals require an exact match and preserve unrelated edges.
    #[test]
    fn remove_edges_matches_exact_values() {
        let from = NodeId::new();
        let to = NodeId::new();
        let other = NodeId::new();
        let dependency = Dependency {
            from,
            to,
            kind: DependencyKind::Backs,
        };
        let retained_dependency = Dependency {
            from,
            to: other,
            kind: DependencyKind::Backs,
        };
        let relation = Relation {
            from,
            to,
            kind: RelationKind::Replicates,
        };
        let retained_relation = Relation {
            from,
            to: other,
            kind: RelationKind::Replicates,
        };
        let mut graph = NodeGraph::new();
        graph.insert_dependency(dependency.clone());
        graph.insert_dependency(retained_dependency.clone());
        graph.insert_relation(relation.clone());
        graph.insert_relation(retained_relation.clone());

        assert!(graph.remove_dependency(&dependency));
        assert!(!graph.remove_dependency(&dependency));
        assert!(graph.remove_relation(&relation));
        assert!(!graph.remove_relation(&relation));
        assert_eq!(
            graph.dependencies().collect::<Vec<_>>(),
            vec![&retained_dependency]
        );
        assert_eq!(
            graph.relations().collect::<Vec<_>>(),
            vec![&retained_relation]
        );
    }
    /// Creates a small graph with independently addressable placeholder nodes.
    fn graph_with_nodes(count: usize) -> (NodeGraph, Vec<NodeId>) {
        let ids: Vec<_> = (0..count).map(|_| NodeId::new()).collect();
        let mut graph = NodeGraph::new();
        for id in &ids {
            assert!(graph.insert_new(*id, disk_node()).is_ok());
        }
        (graph, ids)
    }

    /// New and replacement operations fail without changing the existing graph.
    #[test]
    fn explicit_node_mutations_preserve_state_on_failure() {
        let (mut graph, ids) = graph_with_nodes(1);
        let original = graph.clone();
        let missing = NodeId::new();
        let mut replacement = disk_node();
        replacement.kind.size = Some(Bytes::new(42));
        assert_eq!(
            graph.insert_new(ids[0], replacement.clone()),
            Err(GraphError::DuplicateNode(ids[0]))
        );
        assert_eq!(
            graph.replace_existing(missing, replacement.clone()),
            Err(GraphError::MissingNode(missing))
        );
        assert_eq!(graph, original);
        assert_eq!(
            graph.replace_existing(ids[0], replacement.clone()),
            Ok(disk_node())
        );
        assert_eq!(graph.node(&ids[0]), Some(&replacement));
    }

    /// Tests the raw JSON boundary, before maps and sets could erase duplicates.
    #[test]
    fn deserialize_rejects_duplicate_nodes_and_edges() {
        let id = NodeId::new();
        let node = serde_json::to_string(&disk_node())
            .unwrap_or_else(|error| panic!("node JSON: {error}"));
        let duplicate_nodes = format!(
            r#"{{"nodes":{{"{id}":{node},"{id}":{node}}},"dependencies":[],"relations":[]}}"#,
            id = id.as_uuid()
        );
        let error = serde_json::from_str::<NodeGraph>(&duplicate_nodes)
            .err()
            .unwrap_or_else(|| panic!("duplicate node accepted"));
        assert!(error.to_string().contains("already exists"));

        let (graph, ids) = graph_with_nodes(2);
        let mut value =
            serde_json::to_value(graph).unwrap_or_else(|error| panic!("graph JSON: {error}"));
        for field in ["dependencies", "relations"] {
            let edge = if field == "dependencies" {
                serde_json::json!({"from": ids[0], "to": ids[1], "kind": "Backs"})
            } else {
                serde_json::json!({"from": ids[0], "to": ids[1], "kind": "SnapshotOf"})
            };
            value[field] = serde_json::json!([edge, edge]);
            assert!(serde_json::from_str::<NodeGraph>(&value.to_string()).is_err());
            value[field] = serde_json::json!([]);
        }
    }

    /// Both direct validation and deserialization reject malformed representations.
    #[test]
    fn invalid_structure_is_rejected_at_the_wire_boundary() {
        for case in 0..6 {
            let (mut graph, ids) = graph_with_nodes(3);
            graph.insert_dependency(Dependency {
                from: ids[0],
                to: ids[1],
                kind: DependencyKind::Backs,
            });
            match case {
                0 => {
                    graph.remove_node(&ids[0]);
                }
                1 => {
                    graph.insert_dependency(Dependency {
                        from: ids[1],
                        to: ids[1],
                        kind: DependencyKind::Backs,
                    });
                }
                2 => {
                    graph.insert_dependency(Dependency {
                        from: ids[1],
                        to: ids[0],
                        kind: DependencyKind::Provides,
                    });
                }
                3 => {
                    graph.insert_relation(Relation {
                        from: ids[0],
                        to: NodeId::new(),
                        kind: RelationKind::Replicates,
                    });
                }
                4 => {
                    for from in [ids[0], ids[2]] {
                        graph.insert_dependency(Dependency {
                            from,
                            to: ids[1],
                            kind: DependencyKind::Contains {
                                offset: Bytes::new(0),
                            },
                        });
                    }
                }
                _ => {
                    for offset in [0, 1] {
                        graph.insert_dependency(Dependency {
                            from: ids[0],
                            to: ids[1],
                            kind: DependencyKind::Contains {
                                offset: Bytes::new(offset),
                            },
                        });
                    }
                }
            }
            assert!(graph.validate_structure().is_err(), "case {case}");
            let json =
                serde_json::to_string(&graph).unwrap_or_else(|error| panic!("graph JSON: {error}"));
            assert!(
                serde_json::from_str::<NodeGraph>(&json).is_err(),
                "case {case}"
            );
        }
    }

    /// Typed membership and cache roles cannot be duplicated through different edge payloads.
    #[test]
    fn semantic_roles_are_checked_beyond_edge_equality() {
        for case in 0..3 {
            let (mut graph, ids) = graph_with_nodes(3);
            for from in [ids[0], ids[1]] {
                let kind = match case {
                    0 => DependencyKind::MemberOf(MemberRole::Md(MdMember {
                        role: MdMemberRole::Data,
                        slot: Some(0),
                        data_offset: None,
                    })),
                    1 => DependencyKind::MemberOf(MemberRole::Btrfs(BtrfsMember {
                        devid: NonZeroU64::new(1),
                    })),
                    _ => DependencyKind::Caches(CacheRole::Metadata),
                };
                graph.insert_dependency(Dependency {
                    from,
                    to: ids[2],
                    kind,
                });
            }
            assert_eq!(
                graph.validate_structure(),
                Err(GraphError::ConflictingRole(ids[2]))
            );
        }
    }

    /// Overlapping observed partitions remain readable when their duplicated offsets agree.
    #[test]
    fn overlapping_partitions_are_allowed_but_mismatched_offsets_are_not() {
        let (mut graph, ids) = graph_with_nodes(3);
        for id in &ids[1..] {
            let mut partition = disk_node();
            partition.kind.kind = NodeKind::Partition {
                number: std::num::NonZeroU32::new(1).unwrap_or_else(|| unreachable!()),
                offset: Bytes::new(4096),
                role: None,
                attributes: super::super::PartitionAttributes::default(),
            };
            partition.kind.size = Some(Bytes::new(1024));
            assert!(graph.replace_existing(*id, partition).is_ok());
            graph.insert_dependency(Dependency {
                from: ids[0],
                to: *id,
                kind: DependencyKind::Contains {
                    offset: Bytes::new(4096),
                },
            });
        }
        assert_eq!(graph.validate_structure(), Ok(()));
        let json =
            serde_json::to_string(&graph).unwrap_or_else(|error| panic!("graph JSON: {error}"));
        assert_eq!(
            serde_json::from_str::<NodeGraph>(&json).ok(),
            Some(graph.clone())
        );
        graph.remove_dependency(&Dependency {
            from: ids[0],
            to: ids[1],
            kind: DependencyKind::Contains {
                offset: Bytes::new(4096),
            },
        });
        graph.insert_dependency(Dependency {
            from: ids[0],
            to: ids[1],
            kind: DependencyKind::Contains {
                offset: Bytes::new(8192),
            },
        });
        assert_eq!(
            graph.validate_structure(),
            Err(GraphError::PartitionOffset(ids[1]))
        );
        let json =
            serde_json::to_string(&graph).unwrap_or_else(|error| panic!("graph JSON: {error}"));
        assert!(serde_json::from_str::<NodeGraph>(&json).is_err());
    }

    /// Partial and unsupported observations, multiple providers, and relation cycles stay valid.
    #[test]
    fn partial_observations_and_relation_cycles_roundtrip() {
        let (mut graph, ids) = graph_with_nodes(4);
        let mut missing = disk_node();
        missing.size.presence = super::super::Presence::Missing;
        missing.kind.kind = NodeKind::Unknown("unsupported LVM segment".to_owned());
        assert!(graph.replace_existing(ids[0], missing).is_ok());
        for (from, to) in [(ids[0], ids[2]), (ids[1], ids[2]), (ids[2], ids[3])] {
            graph.insert_dependency(Dependency {
                from,
                to,
                kind: DependencyKind::Provides,
            });
        }
        for (from, to) in [(ids[0], ids[1]), (ids[1], ids[0])] {
            graph.insert_relation(Relation {
                from,
                to,
                kind: RelationKind::Replicates,
            });
        }
        assert_eq!(graph.validate_structure(), Ok(()));
        let order = graph
            .topological_order()
            .unwrap_or_else(|error| panic!("DAG: {error}"));
        for edge in graph.dependencies() {
            assert!(
                order.iter().position(|id| *id == edge.from)
                    < order.iter().position(|id| *id == edge.to)
            );
        }
        assert_eq!(
            graph.ancestors(ids[3]),
            HashSet::from([ids[0], ids[1], ids[2]])
        );
        assert!(graph.ancestors(ids[0]).is_empty());
        let value =
            serde_json::to_value(&graph).unwrap_or_else(|error| panic!("graph JSON: {error}"));
        assert!(value["nodes"].is_object());
        assert_eq!(serde_json::from_value::<NodeGraph>(value).ok(), Some(graph));
    }

    /// Traversal terminates even on an unpublished draft with a dependency cycle.
    #[test]
    fn ancestor_walk_excludes_itself_in_a_cycle() {
        let (mut graph, ids) = graph_with_nodes(2);
        for (from, to) in [(ids[0], ids[1]), (ids[1], ids[0])] {
            graph.insert_dependency(Dependency {
                from,
                to,
                kind: DependencyKind::Backs,
            });
        }
        assert_eq!(graph.ancestors(ids[0]), HashSet::from([ids[1]]));
        assert_eq!(graph.topological_order(), Err(GraphError::DependencyCycle));
    }
    /// A degraded array may retain missing members and unknown slots without inventing health.
    #[test]
    fn degraded_md_members_roundtrip() {
        let (mut graph, ids) = graph_with_nodes(3);
        let mut array = disk_node();
        array.kind.kind = NodeKind::MdArray(crate::model::MdArray {
            personality: crate::model::MdPersonality::Raid(crate::model::MdRaid::Raid1),
            metadata: crate::model::MdMetadata::None,
        });
        assert!(graph.replace_existing(ids[2], array).is_ok());
        let mut missing = disk_node();
        missing.size.presence = crate::model::Presence::Missing;
        assert!(graph.replace_existing(ids[1], missing).is_ok());
        for from in [ids[0], ids[1]] {
            graph.insert_dependency(Dependency {
                from,
                to: ids[2],
                kind: DependencyKind::MemberOf(MemberRole::Md(MdMember {
                    role: MdMemberRole::Data,
                    slot: None,
                    data_offset: None,
                })),
            });
        }
        assert_eq!(graph.validate_structure(), Ok(()));
        let json =
            serde_json::to_string(&graph).unwrap_or_else(|error| panic!("graph JSON: {error}"));
        assert_eq!(serde_json::from_str::<NodeGraph>(&json).ok(), Some(graph));
    }
}
