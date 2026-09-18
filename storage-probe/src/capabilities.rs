use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Registry of system capabilities detected at runtime.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapabilityRegistry {
    pub filesystems: HashMap<String, FilesystemCapabilities>,
    pub partition: PartitionCapabilities,
    pub lvm: Option<LvmCapabilities>,
    pub mdraid: Option<MdCapabilities>,
    pub luks: Option<LuksCapabilities>,
    pub btrfs: Option<BtrfsCapabilities>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilesystemCapabilities {
    pub create: bool,
    pub check: bool,
    pub repair: bool,
    pub grow_online: bool,
    pub grow_offline: bool,
    pub shrink_online: bool,
    pub shrink_offline: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PartitionCapabilities {
    pub gpt: bool,
    pub mbr: bool,
    pub create: bool,
    pub delete: bool,
    pub resize: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LvmCapabilities {
    pub available: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MdCapabilities {
    pub available: bool,
    pub levels: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LuksCapabilities {
    pub available: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtrfsCapabilities {
    pub available: bool,
    pub version: Option<String>,
}

impl CapabilityRegistry {
    /// Detect capabilities by checking for required tools.
    pub fn detect() -> Self {
        let mut registry = Self::default();

        // Filesystem capabilities
        registry.filesystems.insert(
            "ext4".to_string(),
            FilesystemCapabilities {
                create: tool_exists("mkfs.ext4"),
                check: tool_exists("e2fsck"),
                repair: tool_exists("e2fsck"),
                grow_online: tool_exists("resize2fs"),
                grow_offline: tool_exists("resize2fs"),
                shrink_online: false,
                shrink_offline: tool_exists("resize2fs"),
            },
        );

        registry.filesystems.insert(
            "xfs".to_string(),
            FilesystemCapabilities {
                create: tool_exists("mkfs.xfs"),
                check: tool_exists("xfs_repair"),
                repair: tool_exists("xfs_repair"),
                grow_online: tool_exists("xfs_growfs"),
                grow_offline: false,
                shrink_online: false,
                shrink_offline: false, // XFS doesn't support shrinking
            },
        );

        registry.filesystems.insert(
            "swap".to_string(),
            FilesystemCapabilities {
                create: tool_exists("mkswap"),
                check: false,
                repair: false,
                grow_online: false,
                grow_offline: false,
                shrink_online: false,
                shrink_offline: false,
            },
        );

        // Partition capabilities
        registry.partition = PartitionCapabilities {
            gpt: tool_exists("sgdisk") || tool_exists("gdisk"),
            mbr: tool_exists("fdisk") || tool_exists("sfdisk"),
            create: true,
            delete: true,
            resize: false,
        };

        // LVM
        if tool_exists("lvm") || tool_exists("pvcreate") {
            registry.lvm = Some(LvmCapabilities {
                available: true,
                version: get_tool_version("lvm", "--version"),
            });
        }

        // MD RAID
        if tool_exists("mdadm") {
            registry.mdraid = Some(MdCapabilities {
                available: true,
                levels: vec![0, 1, 4, 5, 6, 10],
            });
        }

        // LUKS
        if tool_exists("cryptsetup") {
            registry.luks = Some(LuksCapabilities {
                available: true,
                version: get_tool_version("cryptsetup", "--version"),
            });
        }

        // Btrfs
        if tool_exists("btrfs") {
            registry.btrfs = Some(BtrfsCapabilities {
                available: true,
                version: get_tool_version("btrfs", "version"),
            });
        }

        registry
    }

    /// Check if a filesystem operation is supported.
    pub fn can_create_fs(&self, fs: &str) -> bool {
        self.filesystems
            .get(fs)
            .map(|caps| caps.create)
            .unwrap_or(false)
    }
}

fn tool_exists(tool: &str) -> bool {
    use std::process::Command;
    Command::new("which")
        .arg(tool)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn get_tool_version(tool: &str, args: &str) -> Option<String> {
    use std::process::Command;

    let output = Command::new(tool)
        .args(args.split_whitespace())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout)
        .ok()
        .map(|s| s.trim().to_string())
}
