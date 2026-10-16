//! Native write ABI tests use a private in-memory shared library, never real LVM.

#[path = "support/lvm.rs"]
mod support;

use std::error::Error;
use storage_provider::{LinearLvRequest, LvActivationRequest};
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

/// Only a UUID-qualified, unchanged native LV can reach activation, with no force-like options.
#[test]
fn native_activation_checks_identity_state_and_post_call_evidence() -> Result<(), Box<dyn Error>> {
    for scenario in std::iter::once(0).chain(19..30) {
        let fixture = Fixture::new(scenario)?;
        let backend = fixture.registry.backends().next().ok_or("mock backend")?;
        backend.create_linear_lv(&LinearLvRequest::new("vg", "vg-uuid", "data", 4096)?)?;
        assert_eq!(
            backend.check_lv_activation_support().is_ok(),
            ![28, 29].contains(&scenario),
            "support {scenario}"
        );
        let request = LvActivationRequest::new("vg", "vg-uuid", "data", "new-uuid", 4096)?;
        let result = backend.activate_lv(&request);
        assert_eq!(
            result.is_ok(),
            scenario == 0,
            "activation {scenario}: {result:?}"
        );
        if scenario != 23 {
            let state = backend.probe_lvm()?;
            let invoked = ![21, 22, 28, 29].contains(&scenario);
            assert_eq!(
                state.lvs[0].copy_percent,
                u64::from(invoked),
                "native calls {scenario}"
            );
        }
    }
    let fixture = Fixture::new(0)?;
    let backend = fixture.registry.backends().next().ok_or("mock backend")?;
    backend.create_linear_lv(&LinearLvRequest::new("vg", "vg-uuid", "data", 4096)?)?;
    for (vg_uuid, name, uuid, size) in [
        ("wrong", "data", "new-uuid", 4096),
        ("vg-uuid", "other", "new-uuid", 4096),
        ("vg-uuid", "data", "wrong", 4096),
        ("vg-uuid", "data", "new-uuid", 8192),
    ] {
        assert!(
            backend
                .activate_lv(&LvActivationRequest::new("vg", vg_uuid, name, uuid, size)?)
                .is_err()
        );
        assert_eq!(backend.probe_lvm()?.lvs[0].copy_percent, 0);
    }
    assert!(LvActivationRequest::new("vg", "vg-uuid", "data", "", 4096).is_err());
    assert!(LvActivationRequest::new("vg", "vg-uuid", "data", "new-uuid", 0).is_err());
    Ok(())
}
