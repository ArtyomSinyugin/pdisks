//! Opt-in checks for provider dependencies installed on ALT Linux.

use std::{path::PathBuf, process::Command};

use storage_provider::ProviderRegistry;

/// Finds a provider executable without depending on the caller's `PATH`.
fn tool(name: &str) -> PathBuf {
    ["/usr/bin", "/usr/sbin", "/bin", "/sbin"]
        .into_iter()
        .map(|directory| PathBuf::from(directory).join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("provider tool {name} is not installed"))
}

/// Runs a read-only command and returns its stdout.
fn read_only_stdout(name: &str, arguments: &[&str]) -> Vec<u8> {
    let output = Command::new(tool(name))
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("run {name}: {error}"));
    assert!(
        output.status.success(),
        "{name} {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
#[ignore = "requires the complete ALT Linux provider development toolchain"]
/// Verifies that every planned command-backed provider is installed.
fn planned_provider_commands_are_installed() {
    for name in [
        "lsblk",
        "blkid",
        "sfdisk",
        "fdisk",
        "findmnt",
        "udevadm",
        "dmsetup",
        "pvs",
        "vgs",
        "lvs",
        "mdadm",
        "cryptsetup",
        "btrfs",
        "zpool",
        "zfs",
        "nvme",
        "iscsiadm",
        "multipathd",
        "drbdsetup",
    ] {
        assert!(tool(name).is_file());
    }
}

#[test]
#[ignore = "requires the complete ALT Linux provider development toolchain"]
/// Verifies machine-readable commands that are safe without configured pools.
fn available_machine_readable_probes_return_valid_json() {
    for (name, arguments) in [
        ("lsblk", &["-J", "-b", "-O"][..]),
        (
            "findmnt",
            &["-J", "-b", "-o", "SOURCE,TARGET,FSTYPE,OPTIONS"][..],
        ),
        (
            "pvs",
            &[
                "--reportformat",
                "json",
                "--units",
                "b",
                "--nosuffix",
                "--readonly",
            ][..],
        ),
        (
            "vgs",
            &[
                "--reportformat",
                "json",
                "--units",
                "b",
                "--nosuffix",
                "--readonly",
            ][..],
        ),
        (
            "lvs",
            &[
                "--reportformat",
                "json",
                "--units",
                "b",
                "--nosuffix",
                "--readonly",
            ][..],
        ),
        ("nvme", &["list", "-o", "json"][..]),
        ("nvme", &["list-subsys", "-o", "json"][..]),
    ] {
        let output = read_only_stdout(name, arguments);
        serde_json::from_slice::<serde_json::Value>(&output)
            .unwrap_or_else(|error| panic!("{name} {arguments:?} returned invalid JSON: {error}"));
    }
}

#[test]
#[ignore = "enabled explicitly by an RPM build with PDISKS_PROVIDER_MANIFEST"]
/// Verifies opt-in manifests against dependencies installed by BuildRequires.
fn installed_provider_manifests_match_host() {
    let path = std::env::var_os("PDISKS_PROVIDER_MANIFEST")
        .unwrap_or_else(|| panic!("PDISKS_PROVIDER_MANIFEST is required"));
    let path = PathBuf::from(path);
    ProviderRegistry::validate_dependencies(path)
        .unwrap_or_else(|error| panic!("provider dependency validation failed: {error}"));
}
