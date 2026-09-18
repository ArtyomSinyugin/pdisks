use serde::Deserialize;
use std::collections::HashMap;

/// Raw findmnt JSON structure.
/// Raw findmnt JSON structure.
#[derive(Debug, Deserialize)]
pub struct MountsOutput {
    #[serde(default)]
    pub filesystems: Vec<FilesystemEntry>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FilesystemEntry {
    pub target: String,
    pub source: String,
    pub fstype: String,
    #[serde(default)]
    pub options: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
}

/// Parsed mount information.
#[derive(Debug, Default)]
pub struct MountInfo {
    /// Mounts by device path
    pub mounts_by_device: HashMap<String, Vec<MountEntry>>,
    /// Mounts by target
    pub mounts_by_target: HashMap<String, MountEntry>,
}

#[derive(Debug, Clone)]
pub struct MountEntry {
    pub source: String,
    pub target: String,
    pub fstype: String,
    pub options: Option<String>,
    pub uuid: Option<String>,
}

/// Parse findmnt output.
pub fn parse_findmnt(output: &MountsOutput) -> MountInfo {
    let mut info = MountInfo::default();

    for entry in &output.filesystems {
        let mount = MountEntry {
            source: entry.source.clone(),
            target: entry.target.clone(),
            fstype: entry.fstype.clone(),
            options: entry.options.clone(),
            uuid: entry.uuid.clone(),
        };

        info.mounts_by_device
            .entry(entry.source.clone())
            .or_default()
            .push(mount.clone());

        info.mounts_by_target.insert(entry.target.clone(), mount);
    }

    info
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_findmnt_output() {
        let json = r#"{
            "filesystems": [
                {
                    "target": "/",
                    "source": "/dev/sda2",
                    "fstype": "ext4",
                    "options": "rw,relatime",
                    "uuid": "abc-123"
                },
                {
                    "target": "/home",
                    "source": "/dev/sda2",
                    "fstype": "btrfs",
                    "options": "rw,subvol=@home",
                    "uuid": "def-456"
                }
            ]
        }"#;

        let output: MountsOutput = serde_json::from_str(json).unwrap();
        let info = parse_findmnt(&output);

        assert_eq!(info.mounts_by_device.len(), 1);
        assert_eq!(info.mounts_by_target.len(), 2);

        let sda2_mounts = &info.mounts_by_device["/dev/sda2"];
        assert_eq!(sda2_mounts.len(), 2);
    }
}
