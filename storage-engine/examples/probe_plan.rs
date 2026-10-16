//! Read-only probe -> same-pass capacity -> baseline -> editing session integration.
//! No arguments runs a recorded fixture; one manifest path selects the complete native stack.

use std::{error::Error, path::Path};

use storage_core::model::Bytes;
use storage_engine::{
    HistoricalBaseline,
    editing::{PlanSession, PreviewStatus},
};
use storage_probe::{
    LinuxProbe, NativeLocalProvider, NativeSystemProvider, ProbeSnapshot, ProviderProbeError,
    ProviderState, StateProvider,
};
use storage_provider::{
    AllocationDomain, BackendRegistry, ProviderConnectionKind, RegisteredBackend,
};

/// Bytes in a binary gibibyte for the fixture's synthetic LV request.
const GIB: u64 = 1024 * 1024 * 1024;

/// Adapter returning a recorded publication through the real provider assembly boundary.
struct RecordedProvider<'a> {
    /// Raw capacity records and graph originating from the same synthetic measurement.
    publication: &'a ProbeSnapshot,
}

impl StateProvider for RecordedProvider<'_> {
    /// Stable identity of this explicitly selected fixture adapter.
    fn id(&self) -> &str {
        "recorded-probe"
    }
    /// Exclusive responsibility, preserving the production assembler's conflict checks.
    fn responsibility(&self) -> &str {
        "recorded.current-state"
    }
    /// The fixture exercises the same in-process attachment as native adapters.
    fn connection_kind(&self) -> ProviderConnectionKind {
        ProviderConnectionKind::BuiltIn
    }
    /// Returns a single contribution; no independent second measurement supplies capacity.
    fn probe(&self) -> Result<ProviderState, ProviderProbeError> {
        Ok(ProviderState {
            graph: self.publication.current.graph.clone(),
            mounts: self.publication.current.mounts.entries.clone(),
            environment: Some(self.publication.current.environment.clone()),
            diagnostics: self.publication.current.diagnostics.clone(),
            lvm_vgs: self.publication.lvm_vgs.clone(),
        })
    }
}

/// Resolves one explicitly required backend without substituting an unrelated provider.
fn backend<'a>(
    registry: &'a BackendRegistry,
    id: &str,
) -> Result<&'a RegisteredBackend, Box<dyn Error>> {
    registry
        .backends()
        .find(|backend| backend.manifest().id.as_str() == id)
        .ok_or_else(|| format!("required native backend {id} is unavailable").into())
}

/// Runs the existing full read-only native stack while its registry remains borrowed and live.
fn native_snapshot(manifest: &Path) -> Result<ProbeSnapshot, Box<dyn Error>> {
    let registry = BackendRegistry::load_from(manifest)?;
    let local = NativeLocalProvider::new(
        backend(&registry, "udev-block")?,
        backend(&registry, "fdisk-partitions")?,
        backend(&registry, "blkid-signatures")?,
        backend(&registry, "devmapper-topology")?,
        backend(&registry, "cryptsetup-luks")?,
        backend(&registry, "loop")?,
        backend(&registry, "swap")?,
        backend(&registry, "multipath")?,
        backend(&registry, "lvm")?,
        backend(&registry, "mdraid")?,
    );
    let system = NativeSystemProvider::new(
        local,
        backend(&registry, "libmount-mounts")?,
        backend(&registry, "btrfs-subvolumes")?,
        backend(&registry, "btrfs")?,
        backend(&registry, "zfs")?,
        backend(&registry, "nvme")?,
    );
    let probe = LinuxProbe::from_providers(vec![Box::new(system)]);
    Ok(probe.probe())
}

/// Consumes the graph and records from one publication, without treating generation as global ID.
fn baseline(publication: ProbeSnapshot) -> Result<HistoricalBaseline, Box<dyn Error>> {
    Ok(HistoricalBaseline::from_lvm_observations(
        publication.current,
        &publication.lvm_vgs,
    )?)
}

/// Checks the integration and an explicit new-baseline session, preserving old review artifacts.
fn recorded_workflow() -> Result<(), Box<dyn Error>> {
    let recorded: ProbeSnapshot =
        serde_json::from_str(include_str!("fixtures/probe_snapshot.json"))?;
    let probe = LinuxProbe::from_providers(vec![Box::new(RecordedProvider {
        publication: &recorded,
    })]);
    let first = probe.probe();
    let first_generation = first.generation;
    let observed = baseline(first)?;
    assert_eq!(observed.capacity_evidence.len(), 1);
    assert_eq!(observed.capacity_evidence[0].available.get(), 16384);
    let AllocationDomain::LvmVolumeGroup(vg) = observed.capacity_evidence[0].domain;
    let mut session = PlanSession::new(observed)?;
    session.add_lv(session.revision(), vg, "data", Bytes::new(20 * GIB))?;
    assert!(session.preview().calculated().is_some());
    let saved = session.draft_document();
    let frozen = session.freeze(session.revision())?;
    let frozen_json = frozen.document().canonical_json()?;
    let unchanged = baseline(probe.probe())?;
    assert_ne!(unchanged.id, session.baseline().id);
    assert!(session.baseline().has_same_observations(&unchanged)?);

    let mut changed = recorded.clone();
    changed.lvm_vgs[0].vg.free = 0;
    let other_probe = LinuxProbe::from_providers(vec![Box::new(RecordedProvider {
        publication: &changed,
    })]);
    let next = other_probe.probe();
    assert_eq!(next.generation, first_generation);
    let next = baseline(next)?;
    assert!(!session.baseline().has_same_observations(&next)?);
    // The application reviews this candidate explicitly; no draft is silently rebased.
    let mut new_session = PlanSession::new(next)?;
    assert!(new_session.draft().operations.is_empty());
    new_session.add_lv(
        new_session.revision(),
        vg,
        "data",
        Bytes::new(4 * 1024 * 1024),
    )?;
    assert_eq!(new_session.preview().status(), PreviewStatus::Invalid);
    assert_eq!(session.draft_document(), saved);
    assert_eq!(frozen.document().canonical_json()?, frozen_json);

    let mut unknown = recorded.clone();
    unknown.lvm_vgs.clear();
    let mut unknown = PlanSession::new(baseline(unknown)?)?;
    unknown.add_lv(unknown.revision(), vg, "data", Bytes::new(4 * 1024 * 1024))?;
    assert_eq!(unknown.preview().status(), PreviewStatus::Conditional);
    println!(
        "same-pass VG capacity: 16384 extents; changed baseline opens a separate session; old draft and frozen plan unchanged"
    );
    println!(
        "known zero rejects allocation; missing observations remain conditional; equal local generations do not identify a baseline"
    );
    Ok(())
}

/// Runs the fixture by default, or prints an empty editing preview from an explicitly given manifest.
fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let manifest = args.next();
    if args.next().is_some() {
        return Err("usage: probe_plan [native-provider-manifest.json]".into());
    }
    if let Some(manifest) = manifest {
        let session = PlanSession::new(baseline(native_snapshot(Path::new(&manifest))?)?)?;
        for diagnostic in &session.baseline().snapshot.diagnostics {
            eprintln!("{}: {}", diagnostic.code, diagnostic.message);
        }
        println!("{}", session.preview_document().to_json()?);
        Ok(())
    } else {
        recorded_workflow()
    }
}
