//! Checked size arithmetic errors; graph representation errors belong to the graph model.

use thiserror::Error as ThisError;

/// Ошибка арифметики размеров и диапазонов.
#[derive(ThisError, Debug, PartialEq, Eq)]
pub enum GeometryError {
    /// Переполнение u64 при сложении/умножении размеров.
    #[error("переполнение размера (u64 overflow)")]
    Overflow,

    /// Диапазон выходит за границы устройства.
    #[error("диапазон [{start}..{end}) выходит за границы устройства размером {device_size}")]
    OutOfBounds {
        start: u64,
        end: u64,
        device_size: u64,
    },
}
