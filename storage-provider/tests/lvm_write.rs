//! Native write ABI tests use a private in-memory shared library, never real LVM.

use std::{collections::BTreeSet, error::Error, fs, path::PathBuf, process::Command};
use storage_provider::{
    BACKEND_MANIFEST_VERSION, BackendCapability, BackendId, BackendLibrary, BackendManifest,
    BackendManifests, BackendRegistry, LinearLvRequest,
};

/// Temporary compiled fixture and its trusted registry; native handles remain process-lived.
struct Fixture {
    /// Files owned exclusively by this test instance.
    root: PathBuf,
    /// Only the mock native library is registered.
    registry: BackendRegistry,
}

impl Fixture {
    /// Compiles one isolated native scenario without loading any system storage library.
    fn new(scenario: u32) -> Result<Self, Box<dyn Error>> {
        let root =
            std::env::temp_dir().join(format!("disksgraph-lvm-write-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root)?;
        let source = root.join("fixture.c");
        let library = root.join("fixture.so");
        fs::write(&source, include_str!("fixtures/lvm_write.c"))?;
        let output = Command::new("cc")
            .args([
                "-shared", "-fPIC", "-std=c11", "-Wall", "-Wextra", "-Werror",
            ])
            .arg(format!("-DSCENARIO={scenario}"))
            .arg(&source)
            .arg("-o")
            .arg(&library)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let manifest = root.join("providers.json");
        fs::write(
            &manifest,
            serde_json::to_vec(&BackendManifests {
                manifest_version: BACKEND_MANIFEST_VERSION,
                backends: vec![BackendManifest {
                    id: BackendId::new("mock-lvm").ok_or("backend id")?,
                    library: Some(BackendLibrary {
                        path: library,
                        required_symbols: BTreeSet::new(),
                        auxiliary: vec![],
                    }),
                    executable: None,
                    capabilities: BTreeSet::from([
                        BackendCapability::Probe,
                        BackendCapability::Execute,
                    ]),
                }],
            })?,
        )?;
        Ok(Self {
            root,
            registry: BackendRegistry::load_from(manifest)?,
        })
    }
}

impl Drop for Fixture {
    /// Removes only this fixture's own files; retained handles do not access their paths again.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A successful C return is only a candidate receipt; native identity must also match fresh reads.
#[test]
fn native_create_checks_identity_before_and_after_ffi() -> Result<(), Box<dyn Error>> {
    let request = LinearLvRequest::new("vg", "vg-uuid", "data", 4096)?;
    for scenario in 0..19 {
        let fixture = Fixture::new(scenario)?;
        let backend = fixture.registry.backends().next().ok_or("mock backend")?;
        let support = backend.check_linear_lv_support();
        assert_eq!(
            support.is_ok(),
            scenario != 12 && scenario != 18,
            "support {scenario}"
        );
        let result = backend.create_linear_lv(&request);
        if scenario == 0 {
            assert_eq!(result?, "new-uuid");
        } else {
            assert!(result.is_err(), "scenario {scenario}");
        }
        // The mock VG's free-space delta records calls even when the native mutator reports failure.
        if ![11, 14, 18].contains(&scenario) {
            let state = backend.probe_lvm()?;
            let invoked =
                scenario == 0 || (7..=10).contains(&scenario) || (13..=16).contains(&scenario);
            let expected = if scenario == 3 {
                0
            } else if invoked {
                4096
            } else {
                8192
            };
            assert_eq!(state.vgs[0].free, expected, "mutator count {scenario}");
        }
    }
    Ok(())
}
