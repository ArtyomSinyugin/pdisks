//! Zero-cost newtype-обёртки над размерами для checked арифметики.
//!
//! См. P0-10: ядро использует `Bytes`/`Mib` и единые checked conversions,
//! чтобы переполнение или выход за границы устройства становилось ошибкой,
//! а не тихим wrap в release-сборке.

use serde::{Deserialize, Serialize};

use crate::errors::GeometryError;

/// Количество байтов.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct Bytes(u64);

impl Bytes {
    /// Создаёт значение из сырых байтов.
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    /// Сырое значение в байтах.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Checked-сложение; возвращает [`GeometryError::Overflow`] при переполнении.
    pub fn checked_add(self, other: Bytes) -> Result<Bytes, GeometryError> {
        self.0
            .checked_add(other.0)
            .map(Bytes)
            .ok_or(GeometryError::Overflow)
    }

    /// Checked-умножение; возвращает [`GeometryError::Overflow`] при переполнении.
    pub fn checked_mul(self, other: u64) -> Result<Bytes, GeometryError> {
        self.0
            .checked_mul(other)
            .map(Bytes)
            .ok_or(GeometryError::Overflow)
    }

    /// Checked-сумма списка значений.
    pub fn sum_checked(values: impl IntoIterator<Item = Bytes>) -> Result<Bytes, GeometryError> {
        let mut total = Bytes(0);
        for value in values {
            total = total.checked_add(value)?;
        }
        Ok(total)
    }

    /// Проверяет, что диапазон `[self, self + size)` укладывается в устройство.
    pub fn range_within(self, size: Bytes, device_size: Bytes) -> Result<Bytes, GeometryError> {
        let end = self.checked_add(size)?;
        if end > device_size {
            return Err(GeometryError::OutOfBounds {
                start: self.0,
                end: end.0,
                device_size: device_size.as_u64(),
            });
        }
        Ok(end)
    }
}

impl std::fmt::Display for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Количество мегабайт (MiB).
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Mib(pub u64);

impl Mib {
    /// Создаёт значение из сырых MiB.
    pub const fn new(mib: u64) -> Self {
        Self(mib)
    }

    /// Сырое значение в MiB.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Checked-конвертация в байты; возвращает [`GeometryError::Overflow`].
    pub fn bytes(self) -> Result<Bytes, GeometryError> {
        self.0
            .checked_mul(1024 * 1024)
            .map(Bytes)
            .ok_or(GeometryError::Overflow)
    }

    /// Checked-конвертация в секторы (512 байт).
    pub fn sectors_512(self) -> Result<u64, GeometryError> {
        let bytes = self.bytes()?;
        // MiB всегда делится на 512 без остатка, поэтому здесь нет потери.
        Ok(bytes.0 / 512)
    }

    /// Создаёт значение из секторов (512 байт), округляя вверх до MiB.
    pub fn from_sectors_512(sectors: u64) -> Self {
        Self(sectors.div_ceil(2048))
    }
}

impl std::fmt::Display for Mib {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mib_bytes_overflow_is_error() {
        // u64::MAX / (1024*1024) + 1 переполняет checked_mul.
        let huge = Mib(u64::MAX / (1024 * 1024) + 1);
        assert_eq!(huge.bytes(), Err(GeometryError::Overflow));
    }

    #[test]
    fn bytes_checked_add_overflow_is_error() {
        let a = Bytes(u64::MAX);
        let b = Bytes(1);
        assert_eq!(a.checked_add(b), Err(GeometryError::Overflow));
    }

    #[test]
    fn range_within_detects_out_of_bounds() {
        let start = Bytes(100);
        let size = Bytes(50);
        assert!(start.range_within(size, Bytes(149)).is_err());
        assert_eq!(start.range_within(size, Bytes(150)), Ok(Bytes(150)));
    }

    #[test]
    fn sum_checked_accumulates() {
        let Ok(total) = Bytes::sum_checked([Bytes(1), Bytes(2), Bytes(3)]) else {
            panic!("overflow")
        };
        assert_eq!(total, Bytes(6));
    }
}
