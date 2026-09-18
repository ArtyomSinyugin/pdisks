#![allow(clippy::expect_used)]
use std::path::PathBuf;

use storage_probe::LinuxProbe;

fn fixture_dir(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../tests/fixtures");
    path.push(name);
    path
}

#[test]
fn probe_empty_sata_fixture() {
    let probe = LinuxProbe::with_fixtures(fixture_dir("empty-sata"));
    let snapshot = probe.probe().expect("probe should succeed");

    assert_eq!(snapshot.generation, 1);
    // CurrentState will be populated in future work
}

#[test]
fn probe_nvme_gpt_ext4_fixture() {
    let probe = LinuxProbe::with_fixtures(fixture_dir("nvme-gpt-ext4"));
    let snapshot = probe.probe().expect("probe should succeed");

    assert_eq!(snapshot.generation, 1);
}

#[test]
fn probe_mdraid1_fixture() {
    let probe = LinuxProbe::with_fixtures(fixture_dir("mdraid1"));
    let snapshot = probe.probe().expect("probe should succeed");

    assert_eq!(snapshot.generation, 1);
}

#[test]
fn probe_lvm_simple_fixture() {
    let probe = LinuxProbe::with_fixtures(fixture_dir("lvm-simple"));
    let snapshot = probe.probe().expect("probe should succeed");

    assert_eq!(snapshot.generation, 1);
}

#[test]
fn probe_luks_lvm_ext4_fixture() {
    let probe = LinuxProbe::with_fixtures(fixture_dir("luks-lvm-ext4"));
    let snapshot = probe.probe().expect("probe should succeed");

    assert_eq!(snapshot.generation, 1);
}

#[test]
fn probe_btrfs_single_fixture() {
    let probe = LinuxProbe::with_fixtures(fixture_dir("btrfs-single"));
    let snapshot = probe.probe().expect("probe should succeed");

    assert_eq!(snapshot.generation, 1);
}

#[test]
fn probe_btrfs_multidevice_fixture() {
    let probe = LinuxProbe::with_fixtures(fixture_dir("btrfs-multidevice"));
    let snapshot = probe.probe().expect("probe should succeed");

    assert_eq!(snapshot.generation, 1);
}

#[test]
fn probe_btrfs_subvolumes_fixture() {
    let probe = LinuxProbe::with_fixtures(fixture_dir("btrfs-subvolumes"));
    let snapshot = probe.probe().expect("probe should succeed");

    assert_eq!(snapshot.generation, 1);
}

#[test]
fn probe_mounted_root_fixture() {
    let probe = LinuxProbe::with_fixtures(fixture_dir("mounted-root"));
    let snapshot = probe.probe().expect("probe should succeed");

    assert_eq!(snapshot.generation, 1);
}

#[test]
fn probe_broken_fixture() {
    let probe = LinuxProbe::with_fixtures(fixture_dir("broken"));
    let snapshot = probe.probe().expect("probe should succeed");

    assert_eq!(snapshot.generation, 1);
}
