//! Типизированные ошибки ядра.
//!
//! Ядро не логирует и не продолжает работу при ошибке: оно возвращает
//! [`PDiskError`], а решение, что делать дальше (лог, UI, откат), принимает
//! вызывающая граница (RPC-слой, CLI).

use thiserror::Error as ThisError;

pub type Result<T> = std::result::Result<T, PDiskError>;

/// Ошибка ядра: развёрнутый union по подсистемам.
#[derive(ThisError, Debug)]
pub enum PDiskError {
    /// Не удалось разобрать строку статуса устройства (legacy backend3 wire).
    #[error("не удалось разобрать статус устройства: {0}")]
    ParseStatus(#[from] ParseStatusError),

    /// Не удалось применить план к текущему состоянию.
    #[error("не удалось применить план: {0}")]
    Apply(#[from] ApplyError),

    /// Арифметическая ошибка размеров/диапазонов (переполнение, выход за
    /// границы устройства).
    #[error("{0}")]
    Geometry(#[from] GeometryError),

    /// Сериализация действия в legacy whitespace-протокол backend3 невозможна.
    #[error("не удалось сериализовать действие для backend3: {0}")]
    ActionSerialization(String),
}

/// Ошибка разбора строки статуса (см. [`crate::model::deserialize`]).
#[derive(ThisError, Debug, PartialEq, Eq)]
pub enum ParseStatusError {
    /// Строка не содержит ни одного известного маркера.
    #[error("неизвестное значение статуса: {0:?}")]
    Unknown(String),

    /// Маркер присутствует, но пустой payload (например `raid_device:`).
    #[error("пустой payload у статуса: {0:?}")]
    EmptyPayload(String),
}

/// Ошибка применения плана к текущему состоянию.
#[derive(ThisError, Debug)]
pub enum ApplyError {
    /// Целевое устройство не найдено в CurrentState/FutureState.
    #[error("устройство {0} не найдено в состоянии")]
    MissingTarget(String),

    /// Раздел, который планируется удалить, отсутствует на диске.
    #[error("раздел {disk}{part} отсутствует на диске")]
    PartitionToRemoveMissing { disk: String, part: u32 },

    /// Переполнение или выход диапазона за границы устройства.
    #[error(transparent)]
    Geometry(#[from] GeometryError),
}

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

    /// Нулевой размер создаваемого объекта (раздел/LV/swap).
    #[error("нулевой размер создаваемого объекта {0}")]
    ZeroSized(String),
}
