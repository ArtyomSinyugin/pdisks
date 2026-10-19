//! Recorded C4 runtime fixtures; all storage mutations happen only in Rust memory.

use super::*;
use storage_core::model::FilesystemKind;
use storage_engine::ActionRunState;
use storage_runtime::{
    coordinator::ExecutionCoordinator, lease::ExecutionRoot, targets::resolve_action_with_bindings,
};

/// Builds the roadmap's LV creation, activation and ext4 prefix using normal plan editing.
fn filesystem_plan() -> Result<(ProbeSnapshot, FrozenPlan, NodeId), Box<dyn Error>> {
    let (state, _) = fixture()?;
    let baseline =
        HistoricalBaseline::from_lvm_observations(state.current.clone(), &state.lvm_vgs)?;
    let mut session = PlanSession::new(baseline)?;
    let lv = session
        .add_lv(
            session.revision(),
            state.lvm_vgs[0].node,
            "data",
            Bytes::new(16 * 1024 * 1024),
        )?
        .created_node
        .ok_or("planned LV")?;
    session.add_filesystem(session.revision(), lv, FilesystemKind::Ext4)?;
    Ok((state, session.freeze(session.revision())?, lv))
}

/// Ext4 resolution requires the same run's verified LV plus fresh active writable endpoint evidence.
#[test]
fn ext4_resolution_requires_verified_active_unused_lv() -> Result<(), Box<dyn Error>> {
    let (state, frozen, _) = filesystem_plan()?;
    let state = Rc::new(RefCell::new(state));
    let scratch = admission::Scratch::new()?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let mut fake = coordination::dispatcher(&scratch, state.clone());
    fake.created_uuid = Some("created-lv".into());
    let mut coordinator = ExecutionCoordinator::new(
        ExecutionRoot::open(&scratch.0)?,
        coordination::host(state, &run)?,
        fake,
        &run,
    );
    assert_eq!(
        coordinator.execute_action(&mut run, ActionId::new(0), None)?,
        ActionRunState::Verified
    );
    let inactive = run.latest_observation().ok_or("completion")?;
    assert!(
        resolve_action_with_bindings(
            &run,
            ActionId::new(2),
            inactive,
            backend(),
            coordinator.bindings()
        )
        .is_err()
    );
    assert_eq!(
        coordinator.execute_action(&mut run, ActionId::new(1), None)?,
        ActionRunState::Verified
    );
    let observed = run.latest_observation().ok_or("completion")?;
    let resolved = resolve_action_with_bindings(
        &run,
        ActionId::new(2),
        observed,
        backend(),
        coordinator.bindings(),
    )?;
    assert!(resolved.lv_target().ok_or("LV")?.block.is_some());
    for case in 0..4 {
        let mut changed = observed.clone();
        let id = resolved.lv_target().ok_or("LV")?.observed_id;
        let mut node = changed.snapshot.graph.node(&id).ok_or("node")?.clone();
        match case {
            0 => node.size.lvm_active = None,
            1 => node.size.block = None,
            2 => {
                if let Some(block) = &mut node.size.block {
                    block.read_only = Some(true);
                }
            }
            _ => {
                if let Some(block) = &mut node.size.block {
                    block.paths = vec!["relative".into()];
                }
            }
        }
        changed.snapshot.graph.replace_existing(id, node)?;
        assert!(
            resolve_action_with_bindings(
                &run,
                ActionId::new(2),
                &changed,
                backend(),
                coordinator.bindings()
            )
            .is_err(),
            "case {case}"
        );
    }
    assert!(
        resolve_action_with_bindings(
            &run,
            ActionId::new(2),
            observed,
            backend(),
            &storage_runtime::bindings::OutputBindings::new(&run)
        )
        .is_err()
    );
    Ok(())
}
