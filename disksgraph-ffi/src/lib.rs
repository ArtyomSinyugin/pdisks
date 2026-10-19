#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]

//! Initial synchronous Stabby transport. Core models retain their ordinary Rust representation.
//! Every context owns one worker; close joins it before the library may be unloaded.

mod abi;
mod registry;

/// Version of the four exported signatures, independently checked from the JSON protocol.
pub const ABI_VERSION: u32 = 1;
