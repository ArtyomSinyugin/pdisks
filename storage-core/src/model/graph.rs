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
