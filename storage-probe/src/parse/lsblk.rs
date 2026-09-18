//! Парсер live `lsblk --json --bytes`.
//!
//! P0-01: реальный `lsblk --json` использует корневой массив `blockdevices`;
//! `children` находится внутри устройства. Nullable поля представлены
//! `Option`, размеры парсятся только как байты (live path не принимает
//! human-readable формы).

use serde::{Deserialize, Deserializer, de::Error as _};

/// Ошибка разбора lsblk-размера: live path ожидает только целое число байтов.
#[derive(Debug, thiserror::Error)]
#[error("не удалось разобрать размер lsblk {0:?} как байты")]
pub struct SizeParseError(String);

/// Raw lsblk JSON structure (реальная схема `lsblk --json`).
#[derive(Debug)]
pub struct LsblkOutput {
    /// Все top-level устройства в порядке вывода lsblk.
    pub blockdevices: Vec<LsblkDevice>,
}

impl<'de> Deserialize<'de> for LsblkOutput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            blockdevices: Vec<LsblkDevice>,
        }

        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            blockdevices: raw.blockdevices,
        })
    }
}

/// Одно устройство в выводе lsblk. Nullable поля — `Option` (P0-01).
#[derive(Debug, Deserialize, Clone)]
pub struct LsblkDevice {
    pub name: String,
    /// Абсолютный путь устройства (`/dev/sda`, `/dev/nvme0n1p1`).
    #[serde(default)]
    pub path: Option<String>,
    #[serde(rename = "maj:min")]
    pub maj_min: Option<String>,
    /// Размер в байтах (live `--bytes`); парсится строго как u64.
    #[serde(deserialize_with = "deserialize_size_bytes")]
    pub size: u64,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub mountpoint: Option<String>,
    /// Массив точек монтирования (может содержать `null`).
    #[serde(default)]
    pub mountpoints: Option<Vec<Option<String>>>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub partuuid: Option<String>,
    #[serde(default)]
    pub fstype: Option<String>,
    /// GUID типа раздела (например `c12a7328-f31f-11db-9590-0800200c9a66`).
    #[serde(default)]
    pub parttype: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    /// Тип таблицы разделов диска (`gpt`, `dos`).
    #[serde(default)]
    pub pttype: Option<String>,
    /// Сектор начала раздела (для `part`); строка или число.
    #[serde(default, deserialize_with = "deserialize_u64_lenient")]
    pub start: Option<u64>,
    #[serde(default)]
    pub ro: Option<String>,
    #[serde(default)]
    pub log_sectorsize: Option<String>,
    #[serde(default)]
    pub phys_sectorsize: Option<String>,
    #[serde(default)]
    pub wwn: Option<String>,
    #[serde(default)]
    pub children: Vec<LsblkDevice>,
}

/// Lenient u64 deserializer: принимает и строку, и число (lsblk serializes
/// numbers as strings in some util-linux versions).
fn deserialize_u64_lenient<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| D::Error::custom(format!("u64 field: {n} не является u64"))),
        Some(serde_json::Value::String(s)) if s.is_empty() => Ok(None),
        Some(serde_json::Value::String(s)) => s
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| D::Error::custom(format!("u64 field: {s:?} не является u64"))),
        Some(other) => Err(D::Error::custom(format!(
            "u64 field: ожидается число или строка, получено {other}"
        ))),
    }
}

/// Deserializer размера: только целое число байтов (строка или число).
fn deserialize_size_bytes<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| D::Error::custom(format!("размер lsblk {} не является u64", n))),
        serde_json::Value::String(s) => s
            .trim()
            .parse::<u64>()
            .map_err(|_| D::Error::custom(SizeParseError(s))),
        other => Err(D::Error::custom(format!(
            "размер lsblk должен быть числом или строкой, получено {other}"
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// P0 regression: реальный корневой массив `blockdevices` парсится.
    #[test]
    fn live_lsblk_root_blockdevices_is_parsed() {
        let json = r#"{
            "blockdevices": [
                {
                    "name": "sda",
                    "path": "/dev/sda",
                    "maj:min": "8:0",
                    "size": "107374182400",
                    "ro": "0",
                    "type": "disk",
                    "mountpoint": null,
                    "mountpoints": [],
                    "label": "",
                    "uuid": "",
                    "partuuid": "",
                    "parttype": "",
                    "pttype": "gpt",
                    "fstype": null,
                    "log-sectorsize": "512",
                    "phys-sectorsize": "512",
                    "wwn": "",
                    "children": [
                        {
                            "name": "sda1",
                            "path": "/dev/sda1",
                            "maj:min": "8:1",
                            "size": "536870912",
                            "ro": "0",
                            "type": "part",
                            "mountpoint": "/boot/efi",
                            "mountpoints": ["/boot/efi"],
                            "label": "",
                            "uuid": "A1B2-C3D4",
                            "partuuid": "a1b2c3d4-0001",
                            "parttype": "c12a7328-f31f-11db-9590-0800200c9a66",
                            "start": "2048",
                            "fstype": "vfat",
                            "log-sectorsize": "512",
                            "phys-sectorsize": "512",
                            "wwn": ""
                        }
                    ]
                }
            ]
        }"#;

        let output: LsblkOutput = serde_json::from_str(json).expect("valid live lsblk parses");
        assert_eq!(output.blockdevices.len(), 1);
        assert_eq!(output.blockdevices[0].name, "sda");
        assert_eq!(output.blockdevices[0].size, 107374182400);
        assert_eq!(output.blockdevices[0].children.len(), 1);
        assert_eq!(output.blockdevices[0].children[0].start, Some(2048));
        assert_eq!(output.blockdevices[0].pttype.as_deref(), Some("gpt"));
    }

    /// Старая схема (`block-device` + `children`) больше не принимается:
    /// это защищает от регрессии к фиктивной схеме.
    #[test]
    fn legacy_block_device_schema_is_not_accepted() {
        let json = r#"{
            "block-device": {
                "name": "sda",
                "size": "107374182400",
                "type": "disk"
            }
        }"#;

        let output: LsblkOutput = serde_json::from_str(json).expect("empty blockdevices ok");
        assert!(output.blockdevices.is_empty());
    }

    /// Human-readable размер в live path — ошибка, а не 0.
    #[test]
    fn human_readable_size_is_error_in_live_path() {
        let json = r#"{
            "blockdevices": [
                { "name": "sda", "size": "1TB", "type": "disk" }
            ]
        }"#;

        assert!(serde_json::from_str::<LsblkOutput>(json).is_err());
    }

    /// Неизвестный/пустой размер — ошибка.
    #[test]
    fn empty_size_is_error() {
        let json = r#"{
            "blockdevices": [
                { "name": "sda", "size": "", "type": "disk" }
            ]
        }"#;

        assert!(serde_json::from_str::<LsblkOutput>(json).is_err());
    }
}
