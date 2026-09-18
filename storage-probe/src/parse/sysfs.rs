use serde::Deserialize;
use std::collections::HashMap;

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

/// Parsed sysfs information keyed by device name.
#[derive(Debug, Clone, Default)]
pub struct SysfsInfo {
    pub devices: HashMap<String, SysfsDevice>,
}

impl SysfsOutput {
    pub fn into_info(self) -> SysfsInfo {
        let devices = self
            .devices
            .into_iter()
            .map(|d| (d.name.clone(), d))
            .collect();
        SysfsInfo { devices }
    }
}

impl<'a> SysfsOutput {
    /// Borrowed variant of [`SysfsOutput::into_info`].
    pub fn info_ref(&'a self) -> SysfsRef<'a> {
        SysfsRef { output: self }
    }
}

/// Borrowed view over parsed sysfs data.
pub struct SysfsRef<'a> {
    output: &'a SysfsOutput,
}

impl SysfsRef<'_> {
    pub fn get(&self, name: &str) -> Option<&SysfsDevice> {
        self.output.devices.iter().find(|d| d.name == name)
    }
}
