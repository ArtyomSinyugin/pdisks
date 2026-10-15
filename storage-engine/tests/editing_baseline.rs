//! Offline editing inputs keep capacity units and unresolved live conditions explicit.

use storage_core::model::{Bytes, FilesystemKind, LvmLvKind, NodeId};
use storage_engine::{
    ActionId, CheckStatus, CompositionError, HistoricalBaseline, ProtectedScope,
    SimulationObligation, analyse_plan, compose_plan_with_evidence,
};
use storage_provider::{
    ActionPrerequisite, DependencyReason, FilesystemAction, FragmentAction, FragmentDependency,
    FragmentOrigin, LocalActionId, LvmAction, PlanFragment, ProviderAction,
};

/// Loads a fixed historical VG snapshot with 100 free extents of 4 MiB each.
fn baseline() -> HistoricalBaseline {
    serde_json::from_str(include_str!("fixtures/editing_baseline.json"))
        .unwrap_or_else(|error| panic!("decode editing baseline: {error}"))
}

/// Creates an explicitly ordered fragment without touching any native backend.
fn fragment(subject: NodeId, operations: Vec<ProviderAction>) -> PlanFragment {
    let dependencies = (1..operations.len())
        .map(|index| FragmentDependency {
            before: LocalActionId::new(index as u32 - 1),
            after: LocalActionId::new(index as u32),
            reason: DependencyReason::RequestedOrder,
        })
        .collect();
    PlanFragment {
        actions: operations
            .into_iter()
            .enumerate()
            .map(|(index, operation)| FragmentAction {
                id: LocalActionId::new(index as u32),
                operation,
            })
            .collect(),
        dependencies,
        external_requirements: Vec::new(),
        origin: FragmentOrigin::Change {
            subject,
            description: "offline editing fixture".to_owned(),
        },
        diagnostics: Vec::new(),
    }
}

/// Byte-sized LV requests consume extents cumulatively and retain the exact failed check.
#[test]
fn baseline_capacity_uses_extents_and_is_not_mutated() {
    let baseline = baseline();
    let before = baseline.clone();
    let storage_provider::AllocationDomain::LvmVolumeGroup(vg) =
        baseline.capacity_evidence[0].domain;
    let operations = ["data", "logs"]
        .into_iter()
        .map(|name| {
            ProviderAction::Lvm(LvmAction::CreateLv {
                vg,
                planned_node_id: NodeId::new(),
                name: name.to_owned(),
                size: Bytes::new(60 * 4 * 1024 * 1024),
                layout: LvmLvKind::Linear,
            })
        })
        .collect();
    let error = compose_plan_with_evidence(
        &baseline.snapshot,
        vec![fragment(vg, operations)],
        &baseline.capacity_evidence,
    )
    .err()
    .unwrap_or_else(|| panic!("120 extents exceed 100"));
    assert!(
        matches!(error, CompositionError::ActionRejected { action, checks }
        if action == ActionId::new(1) && checks.iter().any(|check| matches!(
            (&check.prerequisite, &check.status),
            (ActionPrerequisite::CapacityAvailable { .. }, CheckStatus::Violated { .. }),
        )))
    );
    assert_eq!(baseline, before);
}

/// A reproducible forecast keeps destructive approval unknown and never fabricates permission.
#[test]
fn baseline_forecast_is_repeatable_and_conditional() {
    let baseline = baseline();
    let before = baseline.clone();
    let storage_provider::AllocationDomain::LvmVolumeGroup(vg) =
        baseline.capacity_evidence[0].domain;
    let lv = NodeId::new();
    let filesystem = NodeId::new();
    let fragment = fragment(
        lv,
        vec![
            ProviderAction::Lvm(LvmAction::CreateLv {
                vg,
                planned_node_id: lv,
                name: "data".to_owned(),
                size: Bytes::new(60 * 4 * 1024 * 1024),
                layout: LvmLvKind::Linear,
            }),
            ProviderAction::Lvm(LvmAction::SetActivation {
                target: lv,
                active: true,
            }),
            ProviderAction::Filesystem(FilesystemAction::Create {
                backing: lv,
                planned_node_id: filesystem,
                filesystem: FilesystemKind::Ext4,
                label: None,
            }),
        ],
    );
    let plan = compose_plan_with_evidence(
        &baseline.snapshot,
        vec![fragment],
        &baseline.capacity_evidence,
    )
    .unwrap_or_else(|error| panic!("compose offline fixture: {error}"));
    let report = analyse_plan(
        &baseline.snapshot,
        &plan,
        &[],
        &baseline.capacity_evidence,
        &ProtectedScope::default(),
    )
    .unwrap_or_else(|error| panic!("analyse offline fixture: {error}"));
    assert!(report.goal_checks.is_empty());
    assert!(!report.is_complete());
    assert!(
        report
            .unresolved_obligations
            .iter()
            .any(|obligation| matches!(
                obligation,
                SimulationObligation::Prerequisite {
                    prerequisite: ActionPrerequisite::DestructiveApproval { .. },
                    ..
                }
            ))
    );
    let encoded =
        serde_json::to_string(&baseline).unwrap_or_else(|error| panic!("encode baseline: {error}"));
    let decoded: HistoricalBaseline =
        serde_json::from_str(&encoded).unwrap_or_else(|error| panic!("decode baseline: {error}"));
    assert_eq!(
        analyse_plan(
            &decoded.snapshot,
            &plan,
            &[],
            &decoded.capacity_evidence,
            &ProtectedScope::default()
        ),
        Ok(report)
    );
    assert_eq!(baseline, before);
}
