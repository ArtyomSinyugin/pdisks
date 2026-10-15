//! Smoke checks for recorded Linux probe inputs, including degraded storage.

use std::path::PathBuf;

use storage_probe::LinuxProbe;

/// Returns the directory containing one recorded probe scenario.
fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/fixtures")
        .join(name)
}

/// Fixtures must exercise actual assembly, rather than merely returning a snapshot counter.
#[test]
fn recorded_fixtures_produce_storage_graphs() {
    for name in [
        "empty-sata",
        "nvme-gpt-ext4",
        "mdraid1",
        "lvm-simple",
        "luks-lvm-ext4",
        "btrfs-single",
        "btrfs-multidevice",
        "btrfs-subvolumes",
        "mounted-root",
        "broken",
    ] {
        let snapshot = LinuxProbe::with_fixtures(fixture_dir(name)).probe();
        assert_eq!(snapshot.generation, 1, "{name}");
        assert!(
            snapshot.current.diagnostics.is_empty(),
            "{name}: {:?}",
            snapshot.current.diagnostics
        );
        assert!(
            snapshot.current.graph.nodes().len() > 0,
            "{name}: missing storage graph"
        );
        snapshot
            .current
            .graph
            .validate_structure()
            .unwrap_or_else(|error| panic!("{name}: invalid assembled graph: {error}"));
    }
}
