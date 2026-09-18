use std::collections::HashMap;

/// Parsed Btrfs subvolume information.
#[derive(Debug, Default)]
pub struct BtrfsSubvolumes {
    /// Subvolumes: path -> SubvolInfo
    pub subvolumes: HashMap<String, SubvolInfo>,
}

#[derive(Debug)]
pub struct SubvolInfo {
    pub id: u64,
    pub path: String,
    pub quota: Option<u64>,
    pub mountpoint: Option<String>,
}

/// Parse "btrfs subvolume list" output.
pub fn parse_btrfs_subvolumes(content: &str) -> BtrfsSubvolumes {
    let mut subvols = BtrfsSubvolumes::default();

    for line in content.lines() {
        let line = line.trim();

        // Expected format: "ID 256 gen 1024 top level 5 path @"
        if line.starts_with("ID") && line.contains("path ") {
            if let Some((id, path)) = parse_subvol_line(line) {
                subvols.subvolumes.insert(
                    path.clone(),
                    SubvolInfo {
                        id,
                        path,
                        quota: None,
                        mountpoint: None,
                    },
                );
            }
        }
    }

    subvols
}

fn parse_subvol_line(line: &str) -> Option<(u64, String)> {
    let parts: Vec<&str> = line.split_whitespace().collect();

    // Find "path" keyword and get the next token as the path
    let mut i = 0;
    while i < parts.len() {
        if parts[i] == "path" && i + 1 < parts.len() {
            let id = line
                .strip_prefix("ID")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()?;

            return Some((id, parts[i + 1].to_string()));
        }
        i += 1;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_subvolume_list() {
        let content = r#"ID 256 gen 1024 top level 5 path @
ID 257 gen 1024 top level 5 path @home
ID 258 gen 1024 top level 5 path @snapshots"#;

        let subvols = parse_btrfs_subvolumes(content);
        assert_eq!(subvols.subvolumes.len(), 3);

        assert!(subvols.subvolumes.contains_key("@"));
        assert!(subvols.subvolumes.contains_key("@home"));
        assert!(subvols.subvolumes.contains_key("@snapshots"));

        let home = &subvols.subvolumes["@home"];
        assert_eq!(home.id, 257);
    }
}
