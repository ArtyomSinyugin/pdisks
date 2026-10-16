//! Durable journal preparation without a native checker, OS leases, or backend invocation.
//! Run with `cargo run -p storage-engine --example journal_plan -- [NEW_JOURNAL_PATH]`.

use std::{error::Error, path::PathBuf};
use storage_core::model::Bytes;
use storage_engine::{
    ActionId, ActionRunState, ExecutionRun,
    editing::{PlanEdit, PlanSession},
    journal::{DurableJournal, JournalLog, RecoveryStatus},
};

/// Writes only a new ordinary journal file; no live evidence or permissions are manufactured.
fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments.next().map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir().join(format!(
            "pdisks-journal-example-{}.jsonl",
            uuid::Uuid::now_v7()
        ))
    });
    if arguments.next().is_some() {
        return Err("usage: journal_plan [NEW_JOURNAL_PATH]".into());
    }
    let baseline = serde_json::from_str(include_str!("fixtures/editing_baseline.json"))?;
    let mut session = PlanSession::new(baseline)?;
    let storage_provider::AllocationDomain::LvmVolumeGroup(vg) =
        session.baseline().capacity_evidence[0].domain;
    let operation = session
        .add_lv(session.revision(), vg, "data", Bytes::new(16 * 1024 * 1024))?
        .added_operation
        .ok_or("operation missing")?;
    let frozen = session.freeze(session.revision())?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let mut journal = DurableJournal::create(&path, &frozen, &run)?;
    // Later editing cannot replace the journal's frozen ownership/review context.
    session.edit(session.revision(), PlanEdit::Clear)?;
    assert_ne!(session.revision(), journal.header().revision);
    assert_eq!(journal.header().operations[0].operation, operation);
    run.begin_check(ActionId::new(0))?;
    assert_eq!(journal.flush_events(&run)?, 1);
    assert!(
        journal
            .record_dispatch_intent(&run, ActionId::new(0))
            .is_err()
    );
    assert_eq!(run.state(ActionId::new(0))?, ActionRunState::Checking);
    assert!(run.latest_observation().is_none());
    let log = JournalLog::read(&path)?;
    let review = log.review()?;
    assert_eq!(review.run_id, run.run_id());
    assert!(
        review
            .actions
            .iter()
            .all(|action| action.status == RecoveryStatus::NoInvocationRecorded)
    );
    let mut interrupted = std::fs::read(&path)?;
    interrupted.extend_from_slice(b"{\"kind\":\"dispatch_intent\",\"value\":");
    assert!(
        JournalLog::from_bytes(&interrupted)?
            .review()?
            .incomplete_tail
    );
    println!("journal: {}", path.display());
    println!("frozen source map retained; Checking event synced with sequence 1");
    println!(
        "no native checker or leases configured: dispatch intent rejected; no invocation recorded"
    );
    println!("journal review is historical only; torn records never enable automatic retry");
    Ok(())
}
