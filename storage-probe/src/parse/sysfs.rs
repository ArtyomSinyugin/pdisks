use serde::Deserialize;

/// Raw sysfs JSON structure.
#[derive(Debug, Deserialize)]
pub struct SysfsOutput {
    #[serde(default)]
    pub devices: Vec<SysfsDevice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SysfsDevice {
    pub name: String,
    pub path: String,
    pub major: u32,
    pub minor: u32,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub logical_block_size: u32,
    #[serde(default)]
    pub physical_block_size: u32,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub wwn: Option<String>,
    #[serde(default)]
    pub rotational: bool,
    #[serde(default)]
    pub removable: bool,
    #[serde(default)]
    pub read_only: bool,
}
