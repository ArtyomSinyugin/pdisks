//! Parsers for raw CLI outputs into domain model.

pub mod btrfs;
pub mod lsblk;
pub mod lvm;
pub mod md;
pub mod mounts;
pub mod sysfs;

pub use btrfs::parse_btrfs_subvolumes;
pub use lsblk::{LsblkDevice, LsblkOutput};
pub use lvm::parse_lvm;
pub use md::parse_mdstat;
pub use mounts::{MountEntry, MountsOutput};
pub use sysfs::SysfsOutput;
