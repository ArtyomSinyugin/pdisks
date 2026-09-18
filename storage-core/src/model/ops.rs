pub enum GraphOp {
    InsertNode { id: NodeId, node: Node },

    RemoveNode { id: NodeId },

    ReplaceNode { id: NodeId, node: Node },

    InsertEdge { edge: Edge },

    RemoveEdge { edge: Edge },
}

pub struct GraphPatch {
    pub ops: Vec<GraphOp>,
}

impl NodeGraph {
    pub fn apply(&mut self, patch: &GraphPatch) -> Result<(), GraphError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub start: Bytes,
    pub len: Bytes,
}

impl Extent {
    pub fn end(&self) -> Result<Bytes, GeometryError>;
    pub fn overlaps(&self, other: &Self) -> Result<bool, GeometryError>;
    pub fn contains(&self, offset: Bytes) -> Result<bool, GeometryError>;
    pub fn within(self, size: Bytes) -> Result<bool, GeometryError>;
}
