//! Narrow ext4 creation parameters; validated intent alone grants no execution authorization.

use thiserror::Error;
use uuid::Uuid;

/// Ext4 failures never authorize retry because a native call may have partially changed content.
#[derive(Debug, Error)]
pub enum FilesystemWriteError {
    /// The registration did not explicitly opt into execution.
    #[error("backend manifest does not declare execution capability")]
    ExecutionNotDeclared,
    /// Device identity, capacity, UUID or label is not representable by the narrow recipe.
    #[error("invalid native ext4 creation request")]
    InvalidRequest,
    /// The retained descriptor no longer agrees with the admitted device or capacity.
    #[error("native filesystem endpoint differs from the admitted target")]
    TargetChanged,
    /// Fresh content is already present or does not match the requested result.
    #[error("native filesystem signature or result differs from the request")]
    ContentMismatch,
    /// Reading or validating the retained descriptor failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Native support, invocation or readback failed; keep the active checkpoint.
    #[error(transparent)]
    Native(#[from] crate::NativeProbeError),
}

/// Immutable ext4-only request with no force flag, arbitrary options, or caller-supplied path.
/// A concrete backend must independently validate a retained block descriptor before invocation.
#[derive(Debug)]
pub struct Ext4CreateRequest {
    /// Exact admitted capacity of the backing LV, not a predicted filesystem size.
    pub(crate) size: u64,
    /// Canonical new filesystem UUID requested from mkfs and independently checked after it.
    pub(crate) uuid: String,
    /// Nonempty label of at most sixteen UTF-8 bytes, or an explicitly unlabelled filesystem.
    pub(crate) label: Option<String>,
}

impl Ext4CreateRequest {
    /// Validates the narrow recipe without opening a device or loading a library.
    pub fn new(size: u64, uuid: &str, label: Option<&str>) -> Result<Self, FilesystemWriteError> {
        if size == 0
            || !size.is_multiple_of(1024)
            || Uuid::parse_str(uuid)
                .ok()
                .is_none_or(|id| id.is_nil() || id.hyphenated().to_string() != uuid)
            || label
                .is_some_and(|label| label.is_empty() || label.len() > 16 || label.contains('\0'))
        {
            return Err(FilesystemWriteError::InvalidRequest);
        }
        Ok(Self {
            size,
            uuid: uuid.into(),
            label: label.map(str::to_owned),
        })
    }

    /// Rejects wrong type, UUID, label and impossible or overflowing ext4 geometry after invocation.
    pub fn check_result(
        &self,
        fs_type: &str,
        uuid: &str,
        label: Option<&str>,
        block_size: u64,
        block_count: u64,
    ) -> Result<(), FilesystemWriteError> {
        if fs_type != "ext4"
            || uuid != self.uuid
            || label != self.label.as_deref()
            || !matches!(block_size, 1024 | 2048 | 4096)
            || block_count == 0
            || block_size
                .checked_mul(block_count)
                .is_none_or(|size| size > self.size)
        {
            return Err(FilesystemWriteError::ContentMismatch);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backend parameters cannot introduce implicit labels, noncanonical identities or rounded capacity.
    #[test]
    fn ext4_request_rejects_ambiguous_parameters() -> Result<(), Box<dyn std::error::Error>> {
        let uuid = Uuid::now_v7().to_string();
        let request = Ext4CreateRequest::new(16 * 1024 * 1024, &uuid, Some("данные"))?;
        request.check_result("ext4", &uuid, Some("данные"), 4096, 4000)?;
        for label in [
            "",
            "seventeen-letters!",
            "bad\0label",
            "д".repeat(9).as_str(),
        ] {
            assert!(Ext4CreateRequest::new(4096, &uuid, Some(label)).is_err());
        }
        for size in [0, 4097] {
            assert!(Ext4CreateRequest::new(size, &uuid, None).is_err());
        }
        for value in [
            "invalid",
            "00000000-0000-0000-0000-000000000000",
            &uuid.to_uppercase(),
        ] {
            assert!(Ext4CreateRequest::new(4096, value, None).is_err());
        }
        for (kind, actual, label, block_size, count) in [
            ("xfs", uuid.as_str(), Some("данные"), 4096, 4000),
            ("ext4", "different", Some("данные"), 4096, 4000),
            ("ext4", uuid.as_str(), None, 4096, 4000),
            ("ext4", uuid.as_str(), Some("данные"), 4096, u64::MAX),
            ("ext4", uuid.as_str(), Some("данные"), 512, 1),
        ] {
            assert!(
                request
                    .check_result(kind, actual, label, block_size, count)
                    .is_err()
            );
        }
        Ok(())
    }
}
