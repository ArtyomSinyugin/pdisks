//! Narrow native LVM write recipe; construction validates intent but grants no authorization.

use crate::LvmEntry;
use thiserror::Error;

/// A rejected request or observation must never reach the native mutator.
#[derive(Debug, Error)]
pub enum LvmWriteError {
    /// Names, identity, or size cannot be represented by this conservative recipe.
    #[error("invalid native linear LV request")]
    InvalidRequest,
    /// The fresh native VG identity, capacity, or LV namespace differs from admission.
    #[error("native LVM target no longer matches the admitted request")]
    TargetChanged,
    /// The mutator may have run; the caller must retain its checkpoint and probe again.
    #[error(transparent)]
    Native(#[from] crate::NativeProbeError),
}

/// Validated linear-only request bound to an admitted VG UUID and exact byte capacity.
/// No serde implementation exists: saved data cannot bypass validation.
#[derive(Debug)]
pub struct LinearLvRequest {
    /// Native VG endpoint, checked again alongside its UUID before mutation.
    pub(crate) vg_name: String,
    /// Stable admitted VG identity.
    pub(crate) vg_uuid: String,
    /// Requested new LV name within that VG.
    pub(crate) lv_name: String,
    /// Exact requested bytes; native extent rounding is forbidden by preflight.
    pub(crate) size: u64,
}

impl LinearLvRequest {
    /// Validates C-safe names, a nonempty UUID, and positive size without touching storage.
    pub fn new(
        vg_name: &str,
        vg_uuid: &str,
        lv_name: &str,
        size: u64,
    ) -> Result<Self, LvmWriteError> {
        if !valid_name(vg_name)
            || !valid_name(lv_name)
            || lv_name.starts_with("snapshot")
            || lv_name.starts_with("pvmove")
            || [
                "_cdata", "_cmeta", "_corig", "_mimage", "_mlog", "_pmspare", "_rimage", "_rmeta",
                "_tdata", "_tmeta", "_vdata", "_vorigin",
            ]
            .iter()
            .any(|reserved| lv_name.contains(reserved))
            || vg_uuid.is_empty()
            || !vg_uuid
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || size == 0
        {
            return Err(LvmWriteError::InvalidRequest);
        }
        Ok(Self {
            vg_name: vg_name.into(),
            vg_uuid: vg_uuid.into(),
            lv_name: lv_name.into(),
            size,
        })
    }

    /// Requires a unique UUID/name pair, nonexported VG, exact extent sizing, and a free LV name.
    /// This pure check alone grants no authorization or protection against concurrent LVM tools.
    pub fn check_target(&self, state: &LvmEntry) -> Result<(), LvmWriteError> {
        let mut matches = state
            .vgs
            .iter()
            .filter(|vg| vg.name == self.vg_name || vg.uuid == self.vg_uuid);
        let vg = matches.next().ok_or(LvmWriteError::TargetChanged)?;
        if matches.next().is_some()
            || vg.name != self.vg_name
            || vg.uuid != self.vg_uuid
            || vg.exported
            || vg.extent_size == 0
            || self.size % vg.extent_size != 0
            || vg.free < self.size
            || vg.free > vg.size
            || state
                .lvs
                .iter()
                .any(|lv| lv.vg_name == self.vg_name && lv.name == self.lv_name)
        {
            return Err(LvmWriteError::TargetChanged);
        }
        Ok(())
    }
}

/// Accepts a conservative subset of LVM names, excluding paths and option-like names.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 127
        && !name.starts_with('-')
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+_.-".contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Native names and exact extent sizing fail closed before calling any C function.
    #[test]
    fn rejects_invalid_requests_and_changed_targets() -> Result<(), Box<dyn std::error::Error>> {
        for name in [
            "",
            ".",
            "..",
            "-option",
            "a/b",
            "a\0b",
            "snapshot1",
            "pvmove0",
            "x_tmeta",
            "данные",
        ] {
            assert!(
                LinearLvRequest::new("vg", "vg-uuid", name, 4096).is_err(),
                "{name:?}"
            );
        }
        for uuid in ["", " ", "a\0b", "a/b"] {
            assert!(LinearLvRequest::new("vg", uuid, "data", 4096).is_err());
        }
        assert!(LinearLvRequest::new("vg", "uuid", "data", 0).is_err());
        let request = LinearLvRequest::new("vg", "vg-uuid", "data", 4096)?;
        let vg = crate::LvmVgEntry {
            name: "vg".into(),
            uuid: "vg-uuid".into(),
            size: 16384,
            extent_size: 4096,
            free: 8192,
            pv_count: 1,
            exported: false,
        };
        let state = LvmEntry {
            pvs: vec![],
            vgs: vec![vg.clone()],
            lvs: vec![],
        };
        assert!(request.check_target(&state).is_ok());
        for case in 0..7 {
            let mut changed = state.clone();
            match case {
                0 => changed.vgs.clear(),
                1 => changed.vgs.push(vg.clone()),
                2 => changed.vgs[0].uuid = "replacement".into(),
                3 => changed.vgs[0].exported = true,
                4 => changed.vgs[0].extent_size = 8192,
                5 => changed.vgs[0].free = 0,
                _ => changed.vgs[0].extent_size = 0,
            }
            assert!(request.check_target(&changed).is_err(), "case {case}");
        }
        Ok(())
    }
}
