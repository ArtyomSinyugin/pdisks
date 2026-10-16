//! Real Linux advisory lease and journal preparation over a synthetic plan; no storage writes.
//! Run with `cargo run -p storage-engine --example lease_plan -- [EXISTING_PRIVATE_DIRECTORY]`.

/// Demonstrates actual exclusion while incomplete live evidence keeps the run in Checking.
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{fs, os::unix::fs::DirBuilderExt, path::PathBuf};
    use storage_core::model::Bytes;
    use storage_engine::{
        ActionId, ActionRunState, ExecutionRun,
        editing::PlanSession,
        journal::{DurableJournal, JournalLog, RecoveryStatus},
        leases::{ExecutionLease, LeaseError},
    };
    use uuid::Uuid;

    let mut arguments = std::env::args_os().skip(1);
    let supplied = arguments.next().map(PathBuf::from);
    if arguments.next().is_some() {
        return Err("usage: lease_plan [EXISTING_PRIVATE_DIRECTORY]".into());
    }
    let directory = match supplied {
        Some(path) => path,
        None => {
            let path =
                std::env::temp_dir().join(format!("pdisks-leases-example-{}", Uuid::now_v7()));
            fs::DirBuilder::new().mode(0o700).create(&path)?;
            path
        }
    };
    let baseline = serde_json::from_str(include_str!("fixtures/editing_baseline.json"))?;
    let mut session = PlanSession::new(baseline)?;
    let storage_provider::AllocationDomain::LvmVolumeGroup(vg) =
        session.baseline().capacity_evidence[0].domain;
    session.add_lv(session.revision(), vg, "data", Bytes::new(16 * 1024 * 1024))?;
    let frozen = session.freeze(session.revision())?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let journal_path = directory.join(format!("run-{}.jsonl", run.run_id()));
    let mut journal = DurableJournal::create(&journal_path, &frozen, &run)?;
    let action = ActionId::new(0);
    run.begin_check(action)?;
    let guard = ExecutionLease::try_acquire(&directory, &run, action)?;
    journal.flush_events(&run)?;
    let mut contender = ExecutionRun::new(frozen.document())?;
    contender.begin_check(action)?;
    assert!(matches!(
        ExecutionLease::try_acquire(&directory, &contender, action),
        Err(LeaseError::Busy)
    ));
    assert!(guard.record_dispatch_intent(&mut journal, &run).is_err());
    assert_eq!(run.state(action)?, ActionRunState::Checking);
    assert!(run.latest_observation().is_none());
    let review = JournalLog::read(&journal_path)?.review()?;
    assert!(
        review
            .actions
            .iter()
            .all(|action| action.status == RecoveryStatus::NoInvocationRecorded)
    );
    drop(guard);
    let next = ExecutionLease::try_acquire(&directory, &contender, action)?;
    drop(next);
    println!("lease root: {}", directory.display());
    println!("journal: {}", journal_path.display());
    println!("real flock: contender gets Busy until guard drops; persistent lock file retained");
    println!(
        "no actual probe or authorization supplied: run stays Checking, dispatch intent rejected"
    );
    println!("no storage backend invoked; isolated default root is for this example only");
    Ok(())
}

/// Reports platform support explicitly when compiling the portable workspace elsewhere.
#[cfg(not(target_os = "linux"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("execution leases currently require Linux".into())
}
