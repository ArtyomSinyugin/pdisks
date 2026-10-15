use serde::Deserialize;

/// Raw findmnt JSON structure.
#[derive(Debug, Deserialize)]
pub struct MountsOutput {
    /// Explicit mount entries; a missing root is not an empty observation.
    pub filesystems: Vec<FilesystemEntry>,
}

#[cfg(test)]
mod tests {
    use super::MountsOutput;

    /// Missing observations cannot be mistaken for a host with no mounts.
    #[test]
    fn requires_explicit_filesystems_list() {
        assert!(serde_json::from_str::<MountsOutput>(r#"{"filesystems": []}"#).is_ok());
        assert!(serde_json::from_str::<MountsOutput>("{}").is_err());
    }
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
