#![allow(clippy::expect_used)]
//! Integration tests for provider DTO to canonical current-state assembly.

use std::path::PathBuf;

use storage_core::model::{MountSource, NodeKind};
use storage_probe::LinuxProbe;

/// Returns one shared fixture directory.
fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/fixtures")
        .join(name)
}

#[test]
fn empty_disk_provider_output_populates_canonical_graph() {
    let snapshot = LinuxProbe::with_fixtures(fixture_dir("empty-sata"))
        .probe()
        .expect("fixture probe should succeed");

    assert!(
        snapshot
            .current
            .graph
            .nodes()
            .any(|(_, node)| matches!(node.kind.kind, NodeKind::Disk))
    );
    assert!(snapshot.current.diagnostics.is_empty());
}

#[test]
fn findmnt_provider_output_populates_mount_state() {
    let snapshot = LinuxProbe::with_fixtures(fixture_dir("mounted-root"))
        .probe()
        .expect("fixture probe should succeed");

    assert_eq!(snapshot.current.mounts.entries.len(), 2);
    assert!(
        snapshot
            .current
            .mounts
            .entries
            .iter()
            .all(|mount| matches!(mount.source, MountSource::Filesystem(_)))
    );
}

#[test]
fn assembled_current_state_round_trips_through_json() {
    let current = LinuxProbe::with_fixtures(fixture_dir("nvme-gpt-ext4"))
        .probe()
        .expect("fixture probe should succeed")
        .current;
    let json = serde_json::to_vec(&current).expect("current state should serialize");
    let decoded = serde_json::from_slice(&json).expect("current state should deserialize");

    assert_eq!(current, decoded);
}
