//! Storage graph and relationship semantics.

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    num::NonZeroU64,
};

use serde::{Deserialize, Serialize};

use super::{Bytes, Node, NodeId, ZfsMember};
use thiserror::Error;

/// Canonical collection of storage nodes and their relationships.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
}
