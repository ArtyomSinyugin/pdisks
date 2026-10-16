#![forbid(unsafe_code)]

//! Trusted host-side execution infrastructure; native storage writes remain disabled.

#[cfg(target_os = "linux")]
pub mod lease;
