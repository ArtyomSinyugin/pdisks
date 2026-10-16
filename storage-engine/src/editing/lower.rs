//! Small explicit recipes; provider contracts own storage validation and predicted effects.

use storage_core::model::{
    Diagnostic, DiagnosticSeverity, FilesystemKind, LvmLvKind, OperationIntent,
};
use storage_provider::{
    ActionPrerequisite, DependencyReason, ExternalRequirement, FilesystemAction, FragmentAction,
    FragmentDependency, FragmentOrigin, LocalActionId, LuksAction, LvmAction, MountAction,
    PartitionAction, PlanFragment, ProviderAction,
};

use super::{PlanOperation, PlanRequest};

/// Creates a planning diagnostic without inventing a parallel storage error catalogue.
pub(super) fn diagnostic(code: &str, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        code: code.to_owned(),
        severity: DiagnosticSeverity::Error,
        subjects: Vec::new(),
        message: message.into(),
        evidence: None,
        suggested_remedy: None,
    }
}

/// Lowers the initial recipes into a single fragment, retaining preallocated output IDs.
/// Maintenance indices are supplied by the build; unsupported requests never disappear silently.
pub fn lower_operation(
    operation: &PlanOperation,
    intent_index: Option<usize>,
) -> Result<PlanFragment, Vec<Diagnostic>> {
    let fail = |code, message| vec![diagnostic(code, message)];
    if operation.request.creates_node() != operation.created_node.is_some() {
        return Err(fail(
            "draft.created_node",
            "creating requests require an output ID; other requests must not have one",
        ));
    }
    let mut operations = Vec::new();
    let mut requirements = Vec::new();
    let origin = match &operation.request {
        PlanRequest::CreatePartition {
            table,
            number,
            offset,
            size,
            role,
            attributes,
        } => {
            let node = operation
                .created_node
                .ok_or_else(|| fail("draft.created_node", "missing partition output ID"))?;
            operations.push(ProviderAction::Partition(PartitionAction::Create {
                table: *table,
                planned_node_id: node,
                number: *number,
                offset: *offset,
                size: *size,
                role: *role,
                attributes: *attributes,
            }));
            FragmentOrigin::Change {
                subject: node,
                description: "create partition at exact byte extent".to_owned(),
            }
        }
        PlanRequest::FormatLuks {
            backing,
            version,
            credential,
        } => {
            let node = operation
                .created_node
                .ok_or_else(|| fail("draft.created_node", "missing LUKS container output ID"))?;
            operations.push(ProviderAction::Luks(LuksAction::Format {
                target: *backing,
                planned_node_id: node,
                version: *version,
                credential: *credential,
            }));
            requirements.push(ExternalRequirement {
                action: LocalActionId::new(0),
                prerequisite: ActionPrerequisite::Writable(*backing),
            });
            FragmentOrigin::Change {
                subject: node,
                description: "format LUKS container".to_owned(),
            }
        }
        PlanRequest::OpenLuks {
            container,
            mapping_name,
            credential,
            read_only,
        } => {
            let node = operation
                .created_node
                .ok_or_else(|| fail("draft.created_node", "missing dm-crypt mapping output ID"))?;
            operations.push(ProviderAction::Luks(LuksAction::Open {
                container: *container,
                planned_node_id: node,
                mapping_name: mapping_name.clone(),
                credential: *credential,
                read_only: *read_only,
            }));
            FragmentOrigin::Change {
                subject: node,
                description: "open LUKS mapping".to_owned(),
            }
        }
        PlanRequest::CreateLv {
            vg,
            name,
            size,
            layout,
            activate,
        } => {
            if *layout != LvmLvKind::Linear {
                return Err(fail(
                    "draft.unsupported_layout",
                    "only linear LV creation is supported",
                ));
            }
            let node = operation
                .created_node
                .ok_or_else(|| fail("draft.created_node", "missing LV output ID"))?;
            operations.push(ProviderAction::Lvm(LvmAction::CreateLv {
                vg: *vg,
                planned_node_id: node,
                name: name.clone(),
                size: *size,
                layout: layout.clone(),
            }));
            if *activate {
                operations.push(ProviderAction::Lvm(LvmAction::SetActivation {
                    target: node,
                    active: true,
                }));
            }
            FragmentOrigin::Change {
                subject: node,
                description: "create logical volume".to_owned(),
            }
        }
        PlanRequest::ResizeLv { lv, new_size } => {
            operations.push(ProviderAction::Lvm(LvmAction::ResizeLv {
                lv: *lv,
                new_size: *new_size,
            }));
            FragmentOrigin::Change {
                subject: *lv,
                description: "resize logical volume".to_owned(),
            }
        }
        PlanRequest::ResizeFilesystem {
            filesystem,
            new_size,
        } => {
            operations.push(ProviderAction::Filesystem(FilesystemAction::Resize {
                filesystem: *filesystem,
                new_size: *new_size,
            }));
            FragmentOrigin::Change {
                subject: *filesystem,
                description: "resize ext filesystem".to_owned(),
            }
        }
        PlanRequest::DeletePartition { partition } => {
            operations.push(ProviderAction::Partition(PartitionAction::Delete {
                partition: *partition,
            }));
            FragmentOrigin::Change {
                subject: *partition,
                description: "delete selected partition".to_owned(),
            }
        }
        PlanRequest::RemoveLv { lv } => {
            operations.push(ProviderAction::Lvm(LvmAction::RemoveLv { lv: *lv }));
            FragmentOrigin::Change {
                subject: *lv,
                description: "remove selected logical volume".to_owned(),
            }
        }
        PlanRequest::CloseLuks { mapping } => {
            operations.push(ProviderAction::Luks(LuksAction::Close {
                mapping: *mapping,
            }));
            FragmentOrigin::Change {
                subject: *mapping,
                description: "close selected LUKS mapping".to_owned(),
            }
        }
        PlanRequest::CreateFilesystem {
            backing,
            filesystem,
            label,
        } => {
            if *filesystem != FilesystemKind::Ext4 {
                return Err(fail(
                    "draft.unsupported_filesystem",
                    "only ext4 creation is supported",
                ));
            }
            let node = operation
                .created_node
                .ok_or_else(|| fail("draft.created_node", "missing filesystem output ID"))?;
            operations.push(ProviderAction::Filesystem(FilesystemAction::Create {
                backing: *backing,
                planned_node_id: node,
                filesystem: filesystem.clone(),
                label: label.clone(),
            }));
            requirements.push(ExternalRequirement {
                action: LocalActionId::new(0),
                prerequisite: ActionPrerequisite::Writable(*backing),
            });
            FragmentOrigin::Change {
                subject: node,
                description: "create filesystem".to_owned(),
            }
        }
        PlanRequest::Mount(entry) | PlanRequest::PersistMount(entry) => {
            operations.push(ProviderAction::Mount(
                if matches!(operation.request, PlanRequest::Mount(_)) {
                    MountAction::Mount {
                        source: entry.source.clone(),
                        target: entry.target.clone(),
                        options: entry.options.clone(),
                        context: entry.context.clone(),
                    }
                } else {
                    MountAction::Persist {
                        entry: entry.clone(),
                    }
                },
            ));
            FragmentOrigin::Mount {
                target: entry.target.clone(),
                context: entry.context.clone(),
            }
        }
        PlanRequest::Unmount { target, context }
        | PlanRequest::RemovePersistentMount { target, context } => {
            operations.push(ProviderAction::Mount(
                if matches!(operation.request, PlanRequest::Unmount { .. }) {
                    MountAction::Unmount {
                        target: target.clone(),
                        context: context.clone(),
                    }
                } else {
                    MountAction::RemovePersistent {
                        target: target.clone(),
                        context: context.clone(),
                    }
                },
            ));
            FragmentOrigin::Mount {
                target: target.clone(),
                context: context.clone(),
            }
        }
        PlanRequest::SetFilesystemLabel { filesystem, label } => {
            operations.push(ProviderAction::Filesystem(FilesystemAction::SetLabel {
                filesystem: *filesystem,
                label: label.clone(),
            }));
            FragmentOrigin::Change {
                subject: *filesystem,
                description: "set filesystem label".to_owned(),
            }
        }
        PlanRequest::Maintenance(OperationIntent::Check { target, force }) => {
            let index = intent_index.ok_or_else(|| {
                fail(
                    "draft.intent_index",
                    "maintenance request needs an intent index",
                )
            })?;
            operations.push(ProviderAction::Filesystem(FilesystemAction::Check {
                filesystem: *target,
                force: *force,
            }));
            FragmentOrigin::Intent { index }
        }
        PlanRequest::Maintenance(_) => {
            return Err(fail(
                "draft.unsupported_intent",
                "only read-only filesystem Check is supported",
            ));
        }
    };
    let dependencies = if operations.len() == 2 {
        vec![FragmentDependency {
            before: LocalActionId::new(0),
            after: LocalActionId::new(1),
            reason: DependencyReason::RequestedOrder,
        }]
    } else {
        Vec::new()
    };
    Ok(PlanFragment {
        actions: operations
            .into_iter()
            .enumerate()
            .map(|(index, operation)| FragmentAction {
                id: LocalActionId::new(index as u32),
                operation,
            })
            .collect(),
        dependencies,
        external_requirements: requirements,
        origin,
        diagnostics: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use storage_core::model::{Bytes, MountContext, MountEntry, MountSource, NodeId};

    use super::*;

    /// The activation recipe reuses its reserved LV ID and never inserts a resize.
    #[test]
    fn lv_recipe_preserves_ids_and_optional_activation() {
        for activate in [false, true] {
            let operation = PlanOperation::new(PlanRequest::CreateLv {
                vg: NodeId::new(),
                name: "data".to_owned(),
                size: Bytes::new(1024),
                layout: LvmLvKind::Linear,
                activate,
            });
            let fragment = lower_operation(&operation, None)
                .unwrap_or_else(|error| panic!("lower LV: {error:?}"));
            assert_eq!(fragment.actions.len(), if activate { 2 } else { 1 });
            assert!(
                matches!(&fragment.actions[0].operation, ProviderAction::Lvm(LvmAction::CreateLv { planned_node_id, .. }) if Some(*planned_node_id) == operation.created_node)
            );
            assert_eq!(fragment.dependencies.len(), usize::from(activate));
            assert_eq!(lower_operation(&operation, None), Ok(fragment));
        }
    }

    /// Filesystem inputs remain explicit requirements and maintenance retains its exact intent.
    #[test]
    fn filesystem_and_check_recipes_keep_inputs_and_intents() {
        let backing = NodeId::new();
        let operation = PlanOperation::new(PlanRequest::CreateFilesystem {
            backing,
            filesystem: FilesystemKind::Ext4,
            label: Some("data".to_owned()),
        });
        let fragment = lower_operation(&operation, None)
            .unwrap_or_else(|error| panic!("lower filesystem: {error:?}"));
        assert_eq!(
            fragment.external_requirements[0].prerequisite,
            ActionPrerequisite::Writable(backing)
        );
        let check = PlanOperation::new(PlanRequest::Maintenance(OperationIntent::Check {
            target: backing,
            force: true,
        }));
        let fragment = lower_operation(&check, Some(3))
            .unwrap_or_else(|error| panic!("lower check: {error:?}"));
        assert_eq!(fragment.origin, FragmentOrigin::Intent { index: 3 });
        assert_eq!(
            fragment.actions[0].operation,
            ProviderAction::Filesystem(FilesystemAction::Check {
                filesystem: backing,
                force: true
            })
        );
        assert!(lower_operation(&check, None).is_err());
        let reformat = PlanOperation::new(PlanRequest::Maintenance(OperationIntent::Reformat {
            target: backing,
            filesystem: FilesystemKind::Ext4,
        }));
        assert_eq!(
            lower_operation(&reformat, Some(0))
                .err()
                .map(|diagnostics| diagnostics[0].code.clone()),
            Some("draft.unsupported_intent".to_owned())
        );
        let unsupported = PlanOperation::new(PlanRequest::CreateFilesystem {
            backing,
            filesystem: FilesystemKind::Xfs,
            label: None,
        });
        assert!(lower_operation(&unsupported, None).is_err());
        let mut invalid = operation;
        invalid.created_node = None;
        assert!(lower_operation(&invalid, None).is_err());
    }

    /// Mount origins describe paths without fabricating storage-node identities.
    #[test]
    fn mount_recipe_preserves_options_and_round_trips_origin() {
        let entry = MountEntry {
            source: MountSource::Tmpfs,
            target: "/data".into(),
            options: vec!["ro".to_owned(), "rw".to_owned()],
            context: MountContext::Host,
        };
        for request in [
            PlanRequest::Mount(entry.clone()),
            PlanRequest::PersistMount(entry.clone()),
        ] {
            let fragment = lower_operation(&PlanOperation::new(request), None)
                .unwrap_or_else(|error| panic!("lower mount: {error:?}"));
            assert_eq!(
                fragment.origin,
                FragmentOrigin::Mount {
                    target: entry.target.clone(),
                    context: entry.context.clone()
                }
            );
            let json = serde_json::to_string(&fragment)
                .unwrap_or_else(|error| panic!("encode fragment: {error}"));
            assert_eq!(
                serde_json::from_str::<PlanFragment>(&json)
                    .unwrap_or_else(|error| panic!("decode fragment: {error}")),
                fragment
            );
        }
    }
}
