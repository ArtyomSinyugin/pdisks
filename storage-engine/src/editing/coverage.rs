//! Independent request-to-action checks, without repeating storage simulation or arithmetic.

use serde::Serialize;

use storage_core::model::{Diagnostic, FilesystemKind, LvmLvKind, OperationIntent};
use storage_provider::{
    FilesystemAction, LuksAction, LvmAction, MountAction, PartitionAction, ProviderAction,
};

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
            PlanRequest::CreatePartition {
                table,
                number,
                offset,
                size,
                role,
                attributes,
            },
            [
                ProviderAction::Partition(PartitionAction::Create {
                    table: actual_table,
                    planned_node_id,
                    number: actual_number,
                    offset: actual_offset,
                    size: actual_size,
                    role: actual_role,
                    attributes: actual_attributes,
                }),
            ],
        ) => {
            table == actual_table
                && operation.created_node == Some(*planned_node_id)
                && number == actual_number
                && offset == actual_offset
                && size == actual_size
                && role == actual_role
                && attributes == actual_attributes
        }
        (
            PlanRequest::FormatLuks {
                backing,
                version,
                credential,
            },
            [
                ProviderAction::Luks(LuksAction::Format {
                    target,
                    planned_node_id,
                    version: actual_version,
                    credential: actual_credential,
                }),
            ],
        ) => {
            backing == target
                && operation.created_node == Some(*planned_node_id)
                && version == actual_version
                && credential == actual_credential
        }
        (
            PlanRequest::OpenLuks {
                container,
                mapping_name,
                credential,
                read_only,
            },
            [
                ProviderAction::Luks(LuksAction::Open {
                    container: actual_container,
                    planned_node_id,
                    mapping_name: actual_name,
                    credential: actual_credential,
                    read_only: actual_read_only,
                }),
            ],
        ) => {
            container == actual_container
                && operation.created_node == Some(*planned_node_id)
                && mapping_name == actual_name
                && credential == actual_credential
                && read_only == actual_read_only
        }
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

    /// LUKS coverage independently checks identities, credentials, format version, and access mode.
    #[test]
    fn luks_recipes_cover_exact_parameters_without_extra_actions() {
        let input = NodeId::new();
        let credential = storage_provider::CredentialRef::from_uuid(input.as_uuid());
        for request in [
            PlanRequest::FormatLuks {
                backing: input,
                version: storage_core::model::LuksVersion::Luks2,
                credential,
            },
            PlanRequest::OpenLuks {
                container: input,
                mapping_name: "crypt-data".to_owned(),
                credential,
                read_only: true,
            },
        ] {
            let operation = PlanOperation::new(request);
            let fragment = lower_operation(&operation, None)
                .unwrap_or_else(|error| panic!("lower LUKS: {error:?}"));
            assert_eq!(fragment.actions.len(), 1);
            assert_eq!(lower_operation(&operation, None), Ok(fragment.clone()));
            let action = &fragment.actions[0].operation;
            assert!(request_coverage(&operation, &[action]).covered);
            assert!(!request_coverage(&operation, &[]).covered);
            assert!(!request_coverage(&operation, &[action, action]).covered);
            for field in 0..5 {
                let mut changed = action.clone();
                match &mut changed {
                    ProviderAction::Luks(LuksAction::Format {
                        target,
                        planned_node_id,
                        version,
                        credential,
                    }) => match field {
                        0 => *target = NodeId::new(),
                        1 => *planned_node_id = NodeId::new(),
                        2 => *version = storage_core::model::LuksVersion::Luks1,
                        _ => {
                            *credential =
                                storage_provider::CredentialRef::from_uuid(NodeId::new().as_uuid())
                        }
                    },
                    ProviderAction::Luks(LuksAction::Open {
                        container,
                        planned_node_id,
                        mapping_name,
                        credential,
                        read_only,
                    }) => match field {
                        0 => *container = NodeId::new(),
                        1 => *planned_node_id = NodeId::new(),
                        2 => *mapping_name = "other".to_owned(),
                        3 => {
                            *credential =
                                storage_provider::CredentialRef::from_uuid(NodeId::new().as_uuid())
                        }
                        _ => *read_only = false,
                    },
                    _ => panic!("unexpected LUKS recipe"),
                }
                assert!(
                    !request_coverage(&operation, &[&changed]).covered,
                    "field {field}"
                );
            }
        }
    }

    /// Exact partition coverage rejects changes to every parameter and to the reserved identity.
    #[test]
    fn partition_recipe_covers_only_the_exact_request() {
        let operation = PlanOperation::new(PlanRequest::CreatePartition {
            table: NodeId::new(),
            number: std::num::NonZeroU32::new(3)
                .unwrap_or_else(|| panic!("nonzero fixture number")),
            offset: Bytes::new(1024),
            size: Bytes::new(2048),
            role: Some(storage_core::model::UsageRole::Data),
            attributes: storage_core::model::PartitionAttributes {
                gpt: Some(4),
                bootable: Some(false),
            },
        });
        let fragment =
            lower_operation(&operation, None).unwrap_or_else(|error| panic!("fixture: {error:?}"));
        assert_eq!(fragment.actions.len(), 1);
        assert!(fragment.dependencies.is_empty());
        assert_eq!(lower_operation(&operation, None), Ok(fragment.clone()));
        let action = &fragment.actions[0].operation;
        assert!(request_coverage(&operation, &[action]).covered);
        assert!(!request_coverage(&operation, &[]).covered);
        assert!(!request_coverage(&operation, &[action, action]).covered);
        for field in 0..8 {
            let mut changed = action.clone();
            let ProviderAction::Partition(PartitionAction::Create {
                table,
                planned_node_id,
                number,
                offset,
                size,
                role,
                attributes,
            }) = &mut changed
            else {
                panic!("wrong recipe");
            };
            match field {
                0 => *table = NodeId::new(),
                1 => *planned_node_id = NodeId::new(),
                2 => {
                    *number = std::num::NonZeroU32::new(4)
                        .unwrap_or_else(|| panic!("nonzero fixture number"))
                }
                3 => *offset = Bytes::new(1025),
                4 => *size = Bytes::new(2049),
                5 => *role = None,
                6 => attributes.gpt = None,
                _ => attributes.bootable = None,
            }
            assert!(
                !request_coverage(&operation, &[&changed]).covered,
                "field {field}"
            );
        }
        let mut rebound = operation.request.clone();
        let source = operation.request.referenced_nodes()[0];
        let target = NodeId::new();
        rebound.remap_nodes(&std::collections::HashMap::from([(source, target)]));
        assert_eq!(rebound.referenced_nodes(), vec![target]);
    }

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
