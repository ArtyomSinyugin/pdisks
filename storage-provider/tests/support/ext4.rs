//! Shared disk-free native ext4 library fixture for provider and runtime tests.

use std::{collections::BTreeSet, error::Error, fs, path::PathBuf, process::Command};
use storage_provider::{
    BACKEND_MANIFEST_VERSION, BackendCapability, BackendId, BackendLibrary, BackendManifest,
    BackendManifests, BackendRegistry,
};

/// Temporary compiled fixture and its trusted registry; native handles remain process-lived.
pub struct Fixture {
    /// Files owned exclusively by this test instance.
    root: PathBuf,
    /// Only the mock native library is registered.
    pub registry: BackendRegistry,
}

impl Fixture {
    /// Compiles one isolated native scenario without loading any system storage library.
    pub fn new(scenario: u32, execute: bool) -> Result<Self, Box<dyn Error>> {
        let root =
            std::env::temp_dir().join(format!("disksgraph-ext4-write-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root)?;
        let source = root.join("fixture.c");
        let library = root.join("fixture.so");
        fs::write(&source, include_str!("../fixtures/ext4_write.c"))?;
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
                    id: BackendId::new("mock-ext4").ok_or("backend id")?,
                    library: Some(BackendLibrary {
                        path: library,
                        required_symbols: BTreeSet::new(),
                        auxiliary: vec![],
                    }),
                    executable: None,
                    capabilities: if execute {
                        BTreeSet::from([BackendCapability::Probe, BackendCapability::Execute])
                    } else {
                        BTreeSet::from([BackendCapability::Probe])
                    },
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
