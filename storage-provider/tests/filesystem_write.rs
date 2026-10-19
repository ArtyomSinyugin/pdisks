//! Native filesystem tests use private compiled C fixtures and never invoke host mkfs tools.

#[path = "support/ext4.rs"]
mod mock_ext4;

/// Concurrent probes must balance init/close without overlapping a standalone plugin session.
#[test]
fn filesystem_plugin_sessions_are_serialized_across_probes()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = mock_ext4::Fixture::new(0, false)?;
    let backend = fixture.registry.backends().next().ok_or("backend")?;
    std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    for _ in 0..3 {
                        let caps = backend.probe_filesystem_capabilities()?;
                        assert_eq!(caps.len(), 1);
                        assert_eq!(caps[0].filesystem, "ext4");
                    }
                    Ok::<_, storage_provider::NativeProbeError>(())
                })
            })
            .collect();
        for thread in threads {
            thread.join().map_err(|_| "probe thread panicked")??;
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    Ok(())
}

/// Execute opt-in and every native symbol/capability must be present before checkpoint publication.
#[test]
fn ext4_readiness_requires_execute_full_abi_and_native_capabilities()
-> Result<(), Box<dyn std::error::Error>> {
    let probe_only = mock_ext4::Fixture::new(0, false)?;
    assert!(matches!(
        probe_only
            .registry
            .backends()
            .next()
            .ok_or("backend")?
            .check_ext4_create_support(),
        Err(storage_provider::FilesystemWriteError::ExecutionNotDeclared)
    ));
    for scenario in 0..=5 {
        let fixture = mock_ext4::Fixture::new(scenario, true)?;
        let backend = fixture.registry.backends().next().ok_or("backend")?;
        assert_eq!(
            backend.check_ext4_create_support().is_ok(),
            scenario == 0,
            "scenario {scenario}"
        );
    }
    Ok(())
}
