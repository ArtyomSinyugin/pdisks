#![forbid(unsafe_code)]
pub mod errors;
pub mod model;

// Public shortcuts for the canonical model types used by active consumers.
pub use errors::{ApplyError, GeometryError, PDiskError, ParseStatusError, Result};
pub use model::{Bytes, Fs, Node, NodeKind};
