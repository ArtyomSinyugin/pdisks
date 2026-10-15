//! Independent request-to-action checks, without repeating storage simulation or arithmetic.

use serde::Serialize;

use storage_core::model::{Diagnostic, FilesystemKind, LvmLvKind, OperationIntent};
use storage_provider::{FilesystemAction, LvmAction, MountAction, ProviderAction};

use super::{OperationId, PlanOperation, PlanRequest, lower::diagnostic};

/// Whether exactly the allowed recipe implements one user request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestCoverage {
    /// User request checked independently of its resulting graph.
    pub operation: OperationId,
    /// True only when every required action is present and no unrelated action is hidden.
    pub covered: bool,
    /// Explanation retained even when the remaining plan simulates successfully.
    pub diagnostics: Vec<Diagnostic>,
}

/// Checks exact parameters and rejects all extra actions outside the initial supported recipes.
pub(super) fn request_coverage(
    operation: &PlanOperation,
    actions: &[&ProviderAction],
) -> RequestCoverage {
    let covered = match (&operation.request, actions) {
        (
            PlanRequest::CreateLv {
                vg,
                name,
                size,
                layout,
                activate,
            },
            [
                ProviderAction::Lvm(LvmAction::CreateLv {
                    vg: actual_vg,
                    planned_node_id,
                    name: actual_name,
                    size: actual_size,
                    layout: actual_layout,
                }),
                tail @ ..,
            ],
        ) => {
            *layout == LvmLvKind::Linear
                && vg == actual_vg
                && operation.created_node == Some(*planned_node_id)
                && name == actual_name
                && size == actual_size
                && layout == actual_layout
                && if *activate {
                    matches!(tail, [ProviderAction::Lvm(LvmAction::SetActivation { target, active: true })] if target == planned_node_id)
                } else {
                    tail.is_empty()
                }
        }
        (
            PlanRequest::CreateFilesystem {
                backing,
                filesystem,
                label,
            },
            [
                ProviderAction::Filesystem(FilesystemAction::Create {
                    backing: actual_backing,
                    planned_node_id,
                    filesystem: actual_filesystem,
                    label: actual_label,
                }),
            ],
        ) => {
            *filesystem == FilesystemKind::Ext4
                && backing == actual_backing
                && operation.created_node == Some(*planned_node_id)
                && filesystem == actual_filesystem
                && label == actual_label
        }
        (
            PlanRequest::Mount(entry),
            [
                ProviderAction::Mount(MountAction::Mount {
                    source,
                    target,
                    options,
                    context,
                }),
            ],
        ) => {
            &entry.source == source
                && &entry.target == target
                && &entry.options == options
                && &entry.context == context
        }
        (
            PlanRequest::PersistMount(entry),
            [ProviderAction::Mount(MountAction::Persist { entry: actual })],
        ) => entry == actual,
        (
            PlanRequest::Unmount { target, context },
            [
                ProviderAction::Mount(MountAction::Unmount {
                    target: actual_target,
                    context: actual_context,
                }),
            ],
        )
        | (
            PlanRequest::RemovePersistentMount { target, context },
            [
                ProviderAction::Mount(MountAction::RemovePersistent {
                    target: actual_target,
                    context: actual_context,
                }),
            ],
        ) => target == actual_target && context == actual_context,
        (
            PlanRequest::SetFilesystemLabel { filesystem, label },
            [
                ProviderAction::Filesystem(FilesystemAction::SetLabel {
                    filesystem: actual_filesystem,
                    label: actual_label,
                }),
            ],
        ) => filesystem == actual_filesystem && label == actual_label,
        (
            PlanRequest::Maintenance(intent @ OperationIntent::Check { .. }),
            [ProviderAction::Filesystem(FilesystemAction::Check { .. })],
        ) => crate::compose::actions_cover_intent(intent, actions),
        _ => false,
    };
    RequestCoverage {
        operation: operation.id,
        covered,
        diagnostics: if covered {
            Vec::new()
        } else {
            vec![diagnostic(
                "draft.request_uncovered",
                "generated actions do not exactly implement the supported request recipe",
            )]
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editing::lower_operation;
    use storage_core::model::{
        Bytes, FilesystemKind, LvmLvKind, MountContext, MountEntry, MountSource, NodeId,
    };

    /// Independent matching rejects missing actions, wrong parameters, and hidden side effects.
    #[test]
    fn exact_recipe_matching_detects_missing_changed_and_extra_actions() {
        let node = NodeId::new();
        let entry = MountEntry {
            source: MountSource::Filesystem(node),
            target: "/data".into(),
            options: vec!["ro".to_owned(), "rw".to_owned()],
            context: MountContext::Host,
        };
        let requests = vec![
            PlanRequest::CreateLv {
                vg: node,
                name: "data".to_owned(),
                size: Bytes::new(1024),
                layout: LvmLvKind::Linear,
                activate: true,
            },
            PlanRequest::CreateFilesystem {
                backing: node,
                filesystem: FilesystemKind::Ext4,
                label: None,
            },
            PlanRequest::Mount(entry.clone()),
            PlanRequest::PersistMount(entry),
            PlanRequest::Unmount {
                target: "/data".into(),
                context: MountContext::Host,
            },
            PlanRequest::RemovePersistentMount {
                target: "/data".into(),
                context: MountContext::Host,
            },
            PlanRequest::SetFilesystemLabel {
                filesystem: node,
                label: Some("data".to_owned()),
            },
            PlanRequest::Maintenance(OperationIntent::Check {
                target: node,
                force: true,
            }),
        ];
        for request in requests {
            let operation = PlanOperation::new(request);
            let fragment = lower_operation(&operation, Some(0))
                .unwrap_or_else(|error| panic!("lower coverage fixture: {error:?}"));
            let mut actions: Vec<_> = fragment
                .actions
                .iter()
                .map(|action| &action.operation)
                .collect();
            assert!(request_coverage(&operation, &actions).covered);
            assert!(!request_coverage(&operation, &[]).covered);
            let extra = ProviderAction::Filesystem(FilesystemAction::SetLabel {
                filesystem: NodeId::new(),
                label: None,
            });
            actions.push(&extra);
            assert!(!request_coverage(&operation, &actions).covered);
        }
        let operation = PlanOperation::new(PlanRequest::Mount(MountEntry {
            source: MountSource::Tmpfs,
            target: "/data".into(),
            options: vec!["ro".to_owned(), "rw".to_owned()],
            context: MountContext::Host,
        }));
        let wrong = ProviderAction::Mount(MountAction::Mount {
            source: MountSource::Tmpfs,
            target: "/data".into(),
            options: vec!["rw".to_owned(), "ro".to_owned()],
            context: MountContext::Host,
        });
        assert!(!request_coverage(&operation, &[&wrong]).covered);
        let wrong_aspect = ProviderAction::Mount(MountAction::Persist {
            entry: MountEntry {
                source: MountSource::Tmpfs,
                target: "/data".into(),
                options: vec!["ro".to_owned(), "rw".to_owned()],
                context: MountContext::Host,
            },
        });
        assert!(!request_coverage(&operation, &[&wrong_aspect]).covered);
    }
}
