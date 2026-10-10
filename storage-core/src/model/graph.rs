//! Storage graph and relationship semantics.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU64,
};

use serde::{Deserialize, Serialize};

use super::{Bytes, Node, NodeId, ZfsMember};

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

    /// Inserts or replaces a node under its canonical identity.
    pub fn insert_node(&mut self, id: NodeId, node: Node) -> Option<Node> {
        self.nodes.insert(id, node)
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
        graph.insert_node(from, disk_node());
        graph.insert_node(to, disk_node());
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
}
