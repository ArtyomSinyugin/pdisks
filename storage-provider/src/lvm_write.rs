//! Narrow native LVM write recipe; construction validates intent but grants no authorization.

use crate::LvmEntry;
use thiserror::Error;

/// A rejected request or observation must never reach the native mutator.
#[derive(Debug, Error)]
pub enum LvmWriteError {
    /// The trusted manifest did not opt this backend into execution.
    #[error("backend manifest does not declare execution capability")]
    ExecutionNotDeclared,
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
            || !self.size.is_multiple_of(vg.extent_size)
            || vg.free < self.size
            || vg.free > vg.size
            || state
                .lvs
                .iter()
                .any(|lv| lv.vg_name == self.vg_name && lv.name == self.lv_name)
        {
            return Err(LvmWriteError::TargetChanged);
        }
        let members: Vec<_> = state
            .pvs
            .iter()
            .filter(|pv| {
                pv.vg_name.as_deref() == Some(self.vg_name.as_str())
                    || pv.vg_uuid.as_deref() == Some(self.vg_uuid.as_str())
            })
            .collect();
        let mut uuids = std::collections::HashSet::new();
        if vg.pv_count == 0
            || members.len() as u64 != vg.pv_count
            || members.iter().any(|pv| {
                pv.vg_name.as_deref() != Some(self.vg_name.as_str())
                    || pv.vg_uuid.as_deref() != Some(self.vg_uuid.as_str())
                    || pv.missing
                    || pv.uuid.is_empty()
                    || !uuids.insert(&pv.uuid)
                    || pv.size == 0
                    || pv.free > pv.size
            })
        {
            return Err(LvmWriteError::TargetChanged);
        }
        Ok(())
    }

    /// Returns a newly observed unique linear LV UUID; any ambiguity after invocation is an incident.
    pub(crate) fn created_uuid(
        &self,
        before: &LvmEntry,
        after: &LvmEntry,
    ) -> Result<String, LvmWriteError> {
        let mut vgs = after
            .vgs
            .iter()
            .filter(|vg| vg.name == self.vg_name || vg.uuid == self.vg_uuid);
        let vg = vgs.next().ok_or(LvmWriteError::TargetChanged)?;
        if vgs.next().is_some() || vg.name != self.vg_name || vg.uuid != self.vg_uuid || vg.exported
        {
            return Err(LvmWriteError::TargetChanged);
        }
        let mut matches = after
            .lvs
            .iter()
            .filter(|lv| lv.vg_name == self.vg_name && lv.name == self.lv_name);
        let lv = matches.next().ok_or(LvmWriteError::TargetChanged)?;
        if matches.next().is_some()
            || lv.size != self.size
            || lv.segment_type.as_deref() != Some("linear")
            || lv.uuid.is_empty()
            || !lv
                .uuid
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || after
                .lvs
                .iter()
                .filter(|other| other.uuid == lv.uuid)
                .count()
                != 1
            || before.lvs.iter().any(|other| other.uuid == lv.uuid)
        {
            return Err(LvmWriteError::TargetChanged);
        }
        Ok(lv.uuid.clone())
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

    /// A receipt requires a new unique native UUID, exact size/layout, and the same owning VG.
    #[test]
    fn receipt_rejects_reused_or_ambiguous_native_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let request = LinearLvRequest::new("vg", "vg-uuid", "data", 4096)?;
        let before = LvmEntry::default();
        let after = LvmEntry {
            pvs: vec![],
            vgs: vec![crate::LvmVgEntry {
                name: "vg".into(),
                uuid: "vg-uuid".into(),
                size: 16384,
                extent_size: 4096,
                free: 4096,
                pv_count: 1,
                exported: false,
            }],
            lvs: vec![crate::LvmLvEntry {
                name: "data".into(),
                vg_name: "vg".into(),
                uuid: "new-uuid".into(),
                size: 4096,
                segment_type: Some("linear".into()),
                attributes: None,
                origin: None,
                pool: None,
                data: None,
                metadata: None,
                roles: None,
                data_percent: 0,
                metadata_percent: 0,
                copy_percent: 0,
                segments: vec![],
            }],
        };
        assert_eq!(request.created_uuid(&before, &after)?, "new-uuid");
        let mut observed = after.lvs[0].clone();
        for (attributes, expected) in [
            (Some("-wi-a-----"), Some(true)),
            (Some("-wi-------"), Some(false)),
            (Some("-wi-s-----"), None),
            (Some("-wi-I-----"), None),
            (Some("a"), None),
            (Some("-wi-a----?"), None),
            (None, None),
        ] {
            observed.attributes = attributes.map(str::to_owned);
            assert_eq!(observed.activation(), expected, "{attributes:?}");
        }
        assert!(request.created_uuid(&after, &after).is_err());
        for case in 0..8 {
            let mut changed = after.clone();
            match case {
                0 => changed.lvs.clear(),
                1 => changed.lvs.push(after.lvs[0].clone()),
                2 => changed.lvs[0].size = 8192,
                3 => changed.lvs[0].segment_type = Some("thin".into()),
                4 => changed.lvs[0].uuid.clear(),
                5 => changed.lvs[0].vg_name = "foreign".into(),
                6 => changed.vgs[0].uuid = "replacement".into(),
                _ => changed.lvs[0].uuid = "invalid uuid".into(),
            }
            assert!(
                request.created_uuid(&before, &changed).is_err(),
                "case {case}"
            );
        }
        Ok(())
    }

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
            pvs: vec![crate::LvmPvEntry {
                device: "/dev/fixture".into(),
                uuid: "pv-uuid".into(),
                vg_name: Some("vg".into()),
                vg_uuid: Some("vg-uuid".into()),
                size: 16384,
                free: 8192,
                data_offset: 4096,
                missing: false,
            }],
            vgs: vec![vg.clone()],
            lvs: vec![],
        };
        assert!(request.check_target(&state).is_ok());
        for case in 0..11 {
            let mut changed = state.clone();
            match case {
                0 => changed.vgs.clear(),
                1 => changed.vgs.push(vg.clone()),
                2 => changed.vgs[0].uuid = "replacement".into(),
                3 => changed.vgs[0].exported = true,
                4 => changed.vgs[0].extent_size = 8192,
                5 => changed.vgs[0].free = 0,
                6 => changed.vgs[0].extent_size = 0,
                7 => changed.pvs.clear(),
                8 => changed.pvs[0].missing = true,
                9 => changed.pvs[0].vg_uuid = Some("replacement".into()),
                _ => changed.pvs.push(changed.pvs[0].clone()),
            }
            assert!(request.check_target(&changed).is_err(), "case {case}");
        }
        Ok(())
    }
}
