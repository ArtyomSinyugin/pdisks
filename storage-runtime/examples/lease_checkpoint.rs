//! Real Linux advisory lease and rejected admission over a synthetic plan; no storage writes.
//! Run with `cargo run -p storage-runtime --example lease_checkpoint -- [EXISTING_PRIVATE_DIRECTORY]`.

/// Explicit fixture adapter; this exercises probe collection without observing host devices.
#[cfg(target_os = "linux")]
struct RecordedProvider {
    /// One synthetic graph and its same-pass VG measurements.
    publication: storage_probe::ProbeSnapshot,
}

#[cfg(target_os = "linux")]
impl storage_probe::StateProvider for RecordedProvider {
    /// Identifies the example's fixture source explicitly.
    fn id(&self) -> &str {
        "recorded-preflight-example"
    }
    /// Owns the recorded contribution within normal provider assembly.
    fn responsibility(&self) -> &str {
        "recorded.storage"
    }
    /// Uses built-in attachment without pretending to be a native library.
    fn connection_kind(&self) -> storage_provider::ProviderConnectionKind {
        storage_provider::ProviderConnectionKind::BuiltIn
    }
    /// Supplies graph and allocation records together through the ordinary probe boundary.
    fn probe(&self) -> Result<storage_probe::ProviderState, storage_probe::ProviderProbeError> {
        let publication = &self.publication;
        Ok(storage_probe::ProviderState {
            graph: publication.current.graph.clone(),
            mounts: publication.current.mounts.entries.clone(),
            lvm_vgs: publication.lvm_vgs.clone(),
            diagnostics: publication.current.diagnostics.clone(),
            environment: Some(publication.current.environment.clone()),
        })
    }
}

/// Demonstrates actual exclusion and a fresh fixture measurement vetoing historical allocation.
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{fs, os::unix::fs::DirBuilderExt, path::PathBuf};
    use storage_core::model::Bytes;
    use storage_engine::{
        ActionId, ActionRunState, AdmissionReport, ExecutionRun, HistoricalBaseline,
        LiveCheckStatus, editing::PlanSession,
    };
    use storage_runtime::{
        checkpoint::CheckpointStore,
        lease::{ExecutionLease, ExecutionRoot, LeaseError},
        probe::ProbedObservation,
    };
    use uuid::Uuid;

    let mut arguments = std::env::args_os().skip(1);
    let supplied = arguments.next().map(PathBuf::from);
    if arguments.next().is_some() {
        return Err("usage: lease_checkpoint [EXISTING_PRIVATE_DIRECTORY]".into());
    }
    let directory = match supplied {
        Some(path) => path,
        None => {
            let path =
                std::env::temp_dir().join(format!("disksgraph-leases-example-{}", Uuid::now_v7()));
            fs::DirBuilder::new().mode(0o700).create(&path)?;
            path
        }
    };
    let root = ExecutionRoot::open(&directory)?;
    let mut publication: storage_probe::ProbeSnapshot = serde_json::from_str(include_str!(
        "../../storage-engine/examples/fixtures/probe_snapshot.json"
    ))?;
    let baseline = HistoricalBaseline::from_lvm_observations(
        publication.current.clone(),
        &publication.lvm_vgs,
    )?;
    let mut session = PlanSession::new(baseline)?;
    let storage_provider::AllocationDomain::LvmVolumeGroup(vg) =
        session.baseline().capacity_evidence[0].domain;
    session.add_lv(session.revision(), vg, "data", Bytes::new(16 * 1024 * 1024))?;
    let frozen = session.freeze(session.revision())?;
    let mut run = ExecutionRun::new(frozen.document())?;
    let action = ActionId::new(0);
    run.begin_check(action)?;
    let guard = ExecutionLease::try_acquire(&root, &run, action)?;
    let checkpoint = CheckpointStore::new(guard.host_lock());
    if checkpoint.inspect()?.is_some() {
        return Err("unresolved active attempt; inspect the incident before execution".into());
    }
    let mut contender = ExecutionRun::new(frozen.document())?;
    contender.begin_check(action)?;
    assert!(matches!(
        ExecutionLease::try_acquire(&root, &contender, action),
        Err(LeaseError::Busy)
    ));
    // The historical plan fit; this separate fixture measurement reports no remaining capacity.
    publication.lvm_vgs[0].vg.free = 0;
    let probe =
        storage_probe::LinuxProbe::from_providers(vec![Box::new(RecordedProvider { publication })]);
    let publication = ProbedObservation::collect(&probe)?;
    let context = run.context();
    let checks = publication
        .check_prerequisites(
            context.historical_baseline,
            &context.plan.actions()[0]
                .informational_contract
                .prerequisites,
        )?
        .checks;
    assert!(!run.record_admission(AdmissionReport {
        run_id: guard.run_id(),
        action: guard.action(),
        observation: publication.into_observation(),
        applicability: LiveCheckStatus::Unknown {
            required_evidence: "native capability and authorization".into()
        },
        held_resources: guard.held_resources().to_vec(),
        checks,
    })?);
    assert!(checkpoint.inspect()?.is_none());
    assert_eq!(run.state(action)?, ActionRunState::Failed);
    assert_eq!(
        run.latest_observation()
            .ok_or("preflight observation")?
            .reference
            .probe_instance,
        probe.instance_id()
    );
    assert!(
        run.admission(action)?
            .ok_or("preflight admission")?
            .checks
            .iter()
            .any(|check| matches!(check.status, LiveCheckStatus::Violated { .. }))
    );
    drop(guard);
    let next = ExecutionLease::try_acquire(&root, &contender, action)?;
    drop(next);
    println!("lease root: {}", directory.display());
    println!("real flock: contender gets Busy until guard drops; persistent lock file retained");
    println!(
        "recorded probe under lease: fresh free capacity is zero; run Failed, no checkpoint or dispatch"
    );
    println!(
        "no native probe or storage backend invoked; isolated default root is for this example only"
    );
    Ok(())
}

/// Reports platform support explicitly when compiling the portable workspace elsewhere.
#[cfg(not(target_os = "linux"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("execution leases currently require Linux".into())
}
