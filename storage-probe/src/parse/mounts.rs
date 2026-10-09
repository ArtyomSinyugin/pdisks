use serde::Deserialize;

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
