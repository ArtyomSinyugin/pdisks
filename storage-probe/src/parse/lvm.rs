use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Raw LVM JSON structure (from pvs/vgs/lvs with --reportformat json_std).
#[derive(Debug, Default, Deserialize)]
pub struct LvmReport {
    #[serde(rename = "block_device")]
    pub block_device: Vec<LvmSection>,
}

#[derive(Debug, Deserialize)]
pub struct LvmSection {
    pub report: LvmReportData,
}

#[derive(Debug, Deserialize)]
pub struct LvmReportData {
    pub label: String,
    #[serde(default)]
    pub fields: Vec<LvmField>,
    #[serde(default)]
    pub rows: Vec<LvmRow>,
}

#[derive(Debug, Deserialize)]
pub struct LvmField {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct LvmRow {
    pub values: Vec<String>,
}

/// Parsed LVM information.
#[derive(Debug, Default)]
pub struct LvmInfo {
    /// PVs: path -> (vg_name, pv_size)
    pub physical_volumes: HashMap<String, PhysicalVolume>,
    /// VGs: name -> VgInfo
    pub volume_groups: HashMap<String, VolumeGroup>,
    /// LVs: path -> LvInfo
    pub logical_volumes: HashMap<String, LogicalVolume>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhysicalVolume {
    pub path: String,
    pub vg_name: Option<String>,
    pub size: u64,
    #[serde(default)]
    pub pv_uuid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeGroup {
    pub name: String,
    pub uuid: Option<String>,
    pub size: u64,
    pub free: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogicalVolume {
    pub path: String,
    pub vg_name: String,
    pub lv_name: String,
    pub size: u64,
    pub uuid: Option<String>,
}

/// Parse LVM reports (pvs, vgs, lvs) into a unified structure.
pub fn parse_lvm(pvs: &Value, vgs: &Value, lvs: &Value) -> LvmInfo {
    let pvs: LvmReport = serde_json::from_value(pvs.clone()).unwrap_or_default();
    let vgs: LvmReport = serde_json::from_value(vgs.clone()).unwrap_or_default();
    let lvs: LvmReport = serde_json::from_value(lvs.clone()).unwrap_or_default();

    let mut info = LvmInfo::default();

    // Parse PVs
    for section in &pvs.block_device {
        if section.report.label != "pv" {
            continue;
        }

        let field_names: Vec<String> = section
            .report
            .fields
            .iter()
            .map(|f| f.name.clone())
            .collect();

        for row in &section.report.rows {
            let mut values: HashMap<&str, &String> = HashMap::new();
            for (i, field) in field_names.iter().enumerate() {
                if i < row.values.len() {
                    values.insert(field.as_str(), &row.values[i]);
                }
            }

            if let Some(path) = values.get("pv_name") {
                let path = path.to_string();
                let size = parse_size(values.get("pv_size").map(|s| s.as_str()).unwrap_or("0"));
                info.physical_volumes.insert(
                    path.clone(),
                    PhysicalVolume {
                        path,
                        vg_name: values
                            .get("vg_name")
                            .map(|s| s.to_string())
                            .filter(|s| !s.is_empty()),
                        size,
                        pv_uuid: values
                            .get("pv_uuid")
                            .map(|s| s.to_string())
                            .filter(|s| !s.is_empty()),
                    },
                );
            }
        }
    }

    // Parse VGs
    for section in &vgs.block_device {
        if section.report.label != "vg" {
            continue;
        }

        let field_names: Vec<String> = section
            .report
            .fields
            .iter()
            .map(|f| f.name.clone())
            .collect();

        for row in &section.report.rows {
            let mut values: HashMap<&str, &String> = HashMap::new();
            for (i, field) in field_names.iter().enumerate() {
                if i < row.values.len() {
                    values.insert(field.as_str(), &row.values[i]);
                }
            }

            if let Some(name) = values.get("vg_name") {
                let name = name.to_string();
                info.volume_groups.insert(
                    name.clone(),
                    VolumeGroup {
                        name,
                        uuid: values
                            .get("vg_uuid")
                            .map(|s| s.to_string())
                            .filter(|s| !s.is_empty()),
                        size: parse_size(values.get("vg_size").map(|s| s.as_str()).unwrap_or("0")),
                        free: parse_size(values.get("vg_free").map(|s| s.as_str()).unwrap_or("0")),
                    },
                );
            }
        }
    }

    // Parse LVs
    for section in &lvs.block_device {
        if section.report.label != "lv" {
            continue;
        }

        let field_names: Vec<String> = section
            .report
            .fields
            .iter()
            .map(|f| f.name.clone())
            .collect();

        for row in &section.report.rows {
            let mut values: HashMap<&str, &String> = HashMap::new();
            for (i, field) in field_names.iter().enumerate() {
                if i < row.values.len() {
                    values.insert(field.as_str(), &row.values[i]);
                }
            }

            if let (Some(path), Some(vg_name), Some(lv_name)) = (
                values.get("lv_path"),
                values.get("vg_name"),
                values.get("lv_name"),
            ) {
                info.logical_volumes.insert(
                    path.to_string(),
                    LogicalVolume {
                        path: path.to_string(),
                        vg_name: vg_name.to_string(),
                        lv_name: lv_name.to_string(),
                        size: parse_size(values.get("lv_size").map(|s| s.as_str()).unwrap_or("0")),
                        uuid: values
                            .get("lv_uuid")
                            .map(|s| s.to_string())
                            .filter(|s| !s.is_empty()),
                    },
                );
            }
        }
    }

    info
}

fn parse_size(size_str: &str) -> u64 {
    if let Ok(bytes) = size_str.parse::<u64>() {
        return bytes;
    }

    let size_str = size_str.trim();
    let (number, multiplier) =
        if let Some(pos) = size_str.find(|c: char| !c.is_ascii_digit() && c != '.') {
            let num_part = &size_str[..pos];
            let unit_part = &size_str[pos..];

            let mult = match unit_part.to_uppercase().as_str() {
                "B" => 1u64,
                "KB" | "K" => 1024,
                "MB" | "M" => 1024 * 1024,
                "GB" | "G" => 1024 * 1024 * 1024,
                "TB" | "T" => 1024 * 1024 * 1024 * 1024,
                _ => 1,
            };

            (num_part.parse::<f64>().unwrap_or(0.0), mult)
        } else {
            return 0;
        };

    (number * multiplier as f64) as u64
}
