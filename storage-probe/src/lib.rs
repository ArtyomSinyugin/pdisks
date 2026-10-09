#![forbid(unsafe_code)]

//! Storage probe: read-path for collecting device information from the system.

pub mod assemble;
pub mod linux;
pub mod parse;
pub mod snapshot;

pub use assemble::{ProviderProbeError, ProviderState, StateProvider};
pub use linux::{
    LibmountProvider, LinuxProbe, NativeBlockProvider, NativeLocalProvider, NativeMapperProvider,
    NativeRuntimeProvider, NativeSystemProvider,
};
pub use snapshot::ProbeSnapshot;
