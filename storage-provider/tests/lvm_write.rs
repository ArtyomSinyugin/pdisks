//! Native write ABI tests use a private in-memory shared library, never real LVM.

#[path = "support/lvm.rs"]
mod support;

use std::error::Error;
use storage_provider::LinearLvRequest;
use support::Fixture;

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
