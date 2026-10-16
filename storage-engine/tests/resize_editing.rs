//! Existing ext4/LV resize requests retain stable storage identities and explicit preparation.

use std::error::Error;
use storage_core::model::{Bytes, NodeId, NodeKind};
use storage_engine::{
    HistoricalBaseline,
    editing::{EditError, PlanEdit, PlanRequest, PlanSession, PreviewStatus},
};

/// Binary mebibyte for exact synthetic capacities.
const MIB: u64 = 1024 * 1024;

/// Loads the mounted 128 MiB ext4/LV fixture with 16 free extents of 4 MiB.
fn baseline() -> Result<HistoricalBaseline, Box<dyn Error>> {
    Ok(serde_json::from_str(include_str!(
        "../examples/fixtures/resize_baseline.json"
    ))?)
}

/// Selects the existing LV and filesystem without relying on device paths or names.
fn nodes(baseline: &HistoricalBaseline) -> Result<(NodeId, NodeId), Box<dyn Error>> {
    let lv = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::LvmLv(_)).then_some(*id))
        .ok_or("missing LV")?;
    let filesystem = baseline
        .snapshot
        .graph
        .nodes()
        .find_map(|(id, node)| matches!(node.kind.kind, NodeKind::Filesystem { .. }).then_some(*id))
        .ok_or("missing filesystem")?;
    Ok((lv, filesystem))
}

/// Resize edits update only their own target size, preserve history, and reject stale/wrong commands.
#[test]
fn editing_resize_size_keeps_existing_identities() -> Result<(), Box<dyn Error>> {
    let baseline = baseline()?;
    let (lv, filesystem) = nodes(&baseline)?;
    let mut session = PlanSession::new(baseline.clone())?;
    let resize_lv = session
        .edit(
            session.revision(),
            PlanEdit::Add(PlanRequest::ResizeLv {
                lv,
                new_size: Bytes::new(160 * MIB),
            }),
        )?
        .added_operation
        .ok_or("missing operation")?;
    let resize_fs = session
        .edit(
            session.revision(),
            PlanEdit::Add(PlanRequest::ResizeFilesystem {
                filesystem,
                new_size: Bytes::new(160 * MIB),
            }),
        )?
        .added_operation
        .ok_or("missing operation")?;
    assert_ne!(session.preview().status(), PreviewStatus::Invalid);
    let before = session.draft_document();
    session.set_resize_size(session.revision(), resize_lv, Bytes::new(164 * MIB))?;
    session.set_resize_size(session.revision(), resize_fs, Bytes::new(164 * MIB))?;
    for (old, new) in before
        .draft
        .operations
        .iter()
        .zip(&session.draft().operations)
    {
        assert_eq!(old.id, new.id);
        assert_eq!(old.created_node, None);
        assert_eq!(new.created_node, None);
        assert_eq!(
            old.request.referenced_nodes(),
            new.request.referenced_nodes()
        );
    }
    let resized = session.draft_document();
    session.undo(session.revision())?;
    session.redo(session.revision())?;
    assert_eq!(session.draft(), &resized.draft);
    let stale = before.revision;
    assert!(matches!(
        session.set_resize_size(stale, resize_lv, Bytes::new(0)),
        Err(EditError::RevisionConflict { .. })
    ));
    assert_eq!(
        session.set_created_lv_size(session.revision(), resize_lv, Bytes::new(1)),
        Err(EditError::NotCreatedLv(resize_lv))
    );
    let label = session
        .edit(
            session.revision(),
            PlanEdit::Add(PlanRequest::SetFilesystemLabel {
                filesystem,
                label: None,
            }),
        )?
        .added_operation
        .ok_or("missing label operation")?;
    let unchanged = session.draft_document();
    assert_eq!(
        session.set_resize_size(session.revision(), label, Bytes::new(1)),
        Err(EditError::NotResize(label))
    );
    assert_eq!(session.draft_document(), unchanged);
    assert_eq!(session.baseline(), &baseline);
    Ok(())
}
