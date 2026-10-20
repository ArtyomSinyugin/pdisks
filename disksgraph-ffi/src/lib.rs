#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]

//! Stabby transport with one bounded background apply job per context. Core models retain their ordinary Rust representation.
//! Every context owns one worker; close joins it before the library may be unloaded.

mod abi;
mod jobs;
pub mod policy;
mod registry;

/// Version of the four exported signatures, independently checked from the JSON protocol.
pub const ABI_VERSION: u32 = 1;

/// Trusted Rust embedding of the same bounded owner transport; no host object crosses the ABI.
pub mod transport {
    pub use crate::abi::boundary;
    pub use crate::registry::{call, close, open_with, reply, Error, Worker};
}
