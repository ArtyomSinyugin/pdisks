#![forbid(unsafe_code)]

//! Trusted host-side execution infrastructure; native writes require explicit policies and adapters.

#[cfg(target_os = "linux")]
pub mod lease;

pub mod checkpoint;

pub mod probe;

pub mod targets;

pub mod authorization;
pub mod capability;
#[cfg(target_os = "linux")]
pub mod host;

#[cfg(target_os = "linux")]
pub mod coordinator;
pub mod dispatcher;

pub mod bindings;

pub mod native_lvm;

#[cfg(target_os = "linux")]
pub mod incident;

pub mod filesystem;
