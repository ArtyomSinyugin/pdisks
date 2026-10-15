#![forbid(unsafe_code)]
pub mod errors;
pub mod model;

// Public shortcuts for the canonical model types used by active consumers.
pub use errors::GeometryError;
pub use model::{Bytes, FilesystemKind, Node, NodeKind};
