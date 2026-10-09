//! Parsers for raw CLI outputs into domain model.

pub mod lsblk;
pub mod mounts;
pub mod sysfs;

pub use lsblk::{LsblkDevice, LsblkOutput};
pub use mounts::MountsOutput;
pub use sysfs::SysfsOutput;
