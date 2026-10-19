//! Private ABI tests bypass only the real-block type gate and call a C fixture that never writes data.

use crate::dylib::ext4_fixture as mock_ext4;

use super::*;
use crate::LoadedBackendAccess;

/// An owned sparse temporary file used solely for descriptor addressing in native mock calls.
struct DeviceFile(std::path::PathBuf);
impl DeviceFile {
    /// Creates a new private file of the mock's expected size without touching any existing path.
    fn new() -> Result<(Self, File), std::io::Error> {
        let path = std::env::temp_dir().join(format!("disksgraph-fs-fd-{}", uuid::Uuid::now_v7()));
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)?;
        file.set_len(16 * 1024 * 1024)?;
        Ok((Self(path), file))
    }
}
impl Drop for DeviceFile {
    /// Only this test's own file is removed after the borrowed descriptor has been released.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Native ABI uses a parent descriptor path and exact options, not a reopened display path.
#[test]
fn native_ext4_mock_uses_retained_fd_and_checks_exact_readback()
-> Result<(), Box<dyn std::error::Error>> {
    for label in [None, Some("данные")] {
        let fixture = mock_ext4::Fixture::new(0, true)?;
        let backend = fixture.registry.backends().next().ok_or("backend")?;
        let LoadedBackendAccess::Library { library } = &backend.access else {
            return Err("library".into());
        };
        let (path, device) = DeviceFile::new()?;
        let request =
            Ext4CreateRequest::new(16 * 1024 * 1024, &uuid::Uuid::now_v7().to_string(), label)?;
        // The public API must reject regular files; the private C fixture never writes their contents.
        assert!(matches!(
            backend.create_ext4(&device, &request),
            Err(FilesystemWriteError::TargetChanged)
        ));
        let moved = path.0.with_extension("moved");
        std::fs::rename(&path.0, &moved)?;
        let moved = DeviceFile(moved);
        std::fs::write(&path.0, b"replacement")?;
        let actual = create(&library.handle, &device, &request)?;
        assert_eq!(actual, request.uuid);
        assert_eq!(device.metadata()?.len(), 16 * 1024 * 1024);
        assert_eq!(std::fs::read(&path.0)?, b"replacement");
        drop(moved);
    }
    Ok(())
}

/// Preflight errors never reach the mock mutator; every post-call ambiguity rejects success without retry.
#[test]
fn native_ext4_failure_matrix_retains_uncertainty_and_frees_results()
-> Result<(), Box<dyn std::error::Error>> {
    for scenario in 1..=22 {
        let fixture = mock_ext4::Fixture::new(scenario, true)?;
        let backend = fixture.registry.backends().next().ok_or("backend")?;
        let LoadedBackendAccess::Library { library } = &backend.access else {
            return Err("library".into());
        };
        let (_path, device) = DeviceFile::new()?;
        let request =
            Ext4CreateRequest::new(16 * 1024 * 1024, &uuid::Uuid::now_v7().to_string(), None)?;
        let error = create(&library.handle, &device, &request)
            .err()
            .ok_or_else(|| format!("scenario {scenario} accepted"))?;
        assert!(!error.to_string().contains("sensitive"));
    }
    Ok(())
}
