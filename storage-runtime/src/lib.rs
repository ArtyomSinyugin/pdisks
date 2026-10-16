#![forbid(unsafe_code)]

//! Trusted host-side execution infrastructure; native storage writes remain disabled.

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

#[cfg(target_os = "linux")]
pub mod incident;
