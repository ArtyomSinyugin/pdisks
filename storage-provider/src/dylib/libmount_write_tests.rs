//! Private ABI checks use inert C contexts; the public API never mounts a regular file.

use super::*;
use crate::dylib::ext4_fixture as fixture;

/// Covers policy setup, syscall failure and every independent completion flag without host mounts.
#[test]
fn mount_recipe_checks_policy_descriptors_and_all_status_fields()
-> Result<(), Box<dyn std::error::Error>> {
    for scenario in [0, 31, 32, 33, 34, 35, 36] {
        let fixture = fixture::Fixture::new(scenario, true)?;
        let backend = fixture
            .registry
            .backends()
            .next()
            .ok_or("fixture backend")?;
        let crate::LoadedBackendAccess::Library { library } = &backend.access else {
            return Err("library required".into());
        };
        let path = std::env::temp_dir().join(format!("disksgraph-mount-{}", uuid::Uuid::now_v7()));
        let source = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)?;
        let target = std::fs::File::open(std::env::temp_dir())?;
        assert!(
            backend.mount_ext4(&source, &target).is_err(),
            "public block gate: {scenario}"
        );
        assert_eq!(check_mount_support(&library.handle).is_ok(), scenario != 31);
        assert_eq!(
            mount_ext4(&library.handle, &source, &target).is_ok(),
            scenario == 0
        );
        std::fs::remove_file(path)?;
    }
    let fixture = fixture::Fixture::new(0, false)?;
    assert!(matches!(
        fixture
            .registry
            .backends()
            .next()
            .ok_or("backend")?
            .check_ext4_mount_support(),
        Err(crate::MountWriteError::ExecutionNotDeclared)
    ));
    Ok(())
}
