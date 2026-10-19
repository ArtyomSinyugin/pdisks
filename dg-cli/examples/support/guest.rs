//! Destructive integration experiment, restricted to the disposable disk supplied by storage-runtime/tests/vm/run_c4.py.
//! Rust and CLI routes share one facade and explicit guest-only host policies.
//! This isolated guest policy is deliberately not a production authorization policy.

// Shared by edition-2024 CLI and edition-2021 ABI examples, whose rustfmt import ordering differs.
#[rustfmt::skip]
use disksgraph::host::{
    BACKEND_MANIFEST_VERSION, BackendCapability, BackendId, BackendLibrary, BackendManifest,
    BackendManifests, BackendRegistry, LvmVgObservation, ProviderAction, ProviderConnectionKind,
    RegisteredBackend, ExecutionContext, ExecutionRunError,
    LinuxProbe, ProviderProbeError, ProviderState, StateProvider,
    authorization::{ApprovalRequest, AuthorizationContext},
    capability::{BackendCapabilityCheck, OperationAvailability},
    checkpoint::ActiveAttempt,
    dispatcher::{BackendDispatcher, DispatchError, ExecutionReceipt},
    host::LinuxLiveHost,
    lease::ExecutionRoot,
    native_storage::NativeStorageAdapter,
    targets::ResolvedAction,
};
use disksgraph::model::*;
use disksgraph::{ActionId, DisksGraph, LiveCheckStatus, LiveObservation};
use std::{collections::BTreeSet, error::Error, fs, path::Path, process::Command};

/// Read actual sysfs bytes; errors never become synthetic geometry or writable defaults.
fn number(path: impl AsRef<Path>) -> Result<u64, Box<dyn Error>> {
    Ok(fs::read_to_string(path)?.trim().parse()?)
}

/// Refuse execution outside the specially booted, single-disk, network-free test VM.
pub fn guard() -> Result<(), Box<dyn Error>> {
    if !fs::read_to_string("/proc/cmdline")?
        .split_whitespace()
        .any(|arg| arg == "disksgraph.c4=1")
        || fs::read_to_string("/sys/block/sda/device/wwid")?.trim() != "naa.5000c40000000001"
        || number("/sys/block/sda/size")? != 1_048_576
        || fs::read_dir("/sys/block")?
            .filter_map(Result::ok)
            .any(|entry| entry.file_name() != "sda" && entry.path().join("device").exists())
        || fs::read_dir("/sys/class/net")?.count() != 1
    {
        return Err("requires the isolated C4 guest and its sole 512 MiB test disk".into());
    }
    Ok(())
}

/// Executes explicit guest-only fixture setup; planned operations use the native coordinator below.
pub fn setup(program: &str, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new(program).args(args).status()?;
    if !status.success() {
        return Err(format!("fixture setup failed: {program} {status}").into());
    }
    Ok(())
}

/// Registers only the four actual native libraries this bounded vertical slice consumes.
fn registry() -> Result<BackendRegistry, Box<dyn Error>> {
    let mut backends = Vec::new();
    for (id, library, execute) in [
        ("vm-lvm", "libbd_lvm.so.3", true),
        ("vm-ext4", "libbd_fs.so.3", true),
        ("vm-blkid", "libblkid.so.1", false),
        ("vm-mount", "libmount.so.1", true),
    ] {
        let mut capabilities = BTreeSet::from([BackendCapability::Probe]);
        if execute {
            capabilities.insert(BackendCapability::Execute);
        }
        backends.push(BackendManifest {
            id: BackendId::new(id).ok_or("backend ID")?,
            library: Some(BackendLibrary {
                path: format!("/usr/lib64/{library}").into(),
                required_symbols: BTreeSet::new(),
                auxiliary: vec![],
            }),
            executable: None,
            capabilities,
        });
    }
    fs::write(
        "/run/providers.json",
        serde_json::to_vec(&BackendManifests {
            manifest_version: BACKEND_MANIFEST_VERSION,
            backends,
        })?,
    )?;
    Ok(BackendRegistry::load_from("/run/providers.json")?)
}

/// Guest-only topology adapter: native LVM/blkid plus actual sysfs endpoints, never planned facts.
struct GuestProbe<'a> {
    /// Actual LVM query registration.
    lvm: &'a RegisteredBackend,
    /// Independent filesystem identity query registration.
    blkid: &'a RegisteredBackend,
    /// Actual Host mount-table registration.
    mount: &'a RegisteredBackend,
}

/// Reads one real block endpoint and its kernel geometry.
fn block(name: &str) -> Result<(Bytes, BlockFacts), Box<dyn Error>> {
    let root = format!("/sys/class/block/{name}");
    let device = fs::read_to_string(format!("{root}/dev"))?;
    let (major, minor) = device.trim().split_once(':').ok_or("device number")?;
    let size = Bytes::new(
        number(format!("{root}/size"))?
            .checked_mul(512)
            .ok_or("size")?,
    );
    Ok((
        size,
        BlockFacts {
            paths: vec![format!("/dev/{name}").into()],
            devno: DeviceNumber {
                major: major.parse()?,
                minor: minor.parse()?,
            },
            geometry: Some(BlockGeometry {
                logical_block_size: BlockSize::new(
                    number(format!("{root}/queue/logical_block_size"))?.try_into()?,
                )
                .ok_or("block size")?,
                physical_block_size: BlockSize::new(
                    number(format!("{root}/queue/physical_block_size"))?.try_into()?,
                ),
                alignment_offset: Some(Bytes::new(number(format!("{root}/alignment_offset"))?)),
                minimum_io_size: Some(Bytes::new(number(format!("{root}/queue/minimum_io_size"))?)),
                optimal_io_size: Some(Bytes::new(number(format!("{root}/queue/optimal_io_size"))?)),
            }),
            read_only: Some(match number(format!("{root}/ro"))? {
                0 => false,
                1 => true,
                _ => return Err("invalid read-only fact".into()),
            }),
        },
    ))
}

/// Inserts a present observed node with an independently measured identity and optional endpoint.
fn insert(
    graph: &mut NodeGraph,
    kind: NodeKind,
    size: Option<Bytes>,
    identity: ExternalId,
    block: Option<BlockFacts>,
    active: Option<bool>,
) -> Result<NodeId, Box<dyn Error>> {
    let id = NodeId::new();
    graph.insert_new(
        id,
        Node {
            kind: NodeSpec { kind, size },
            size: NodeFacts {
                presence: Presence::Present,
                identities: vec![identity],
                block,
                lvm_active: active,
                ..NodeFacts::default()
            },
        },
    )?;
    Ok(id)
}

impl GuestProbe<'_> {
    /// Rebuilds the whole private disk graph from a single fresh native LVM pass.
    fn collect(&self) -> Result<ProviderState, Box<dyn Error>> {
        let native = self.lvm.probe_lvm()?;
        let [vg] = native.vgs.as_slice() else {
            return Err("expected exactly one private VG".into());
        };
        let [pv] = native.pvs.as_slice() else {
            return Err("expected exactly one private PV".into());
        };
        if vg.name != "dg_c4" || pv.device != Path::new("/dev/sda") {
            return Err("foreign native topology".into());
        }
        let mut state = ProviderState::default();
        let (size, facts) = block("sda")?;
        let disk = insert(
            &mut state.graph,
            NodeKind::Disk,
            Some(size),
            ExternalId::Wwn(
                fs::read_to_string("/sys/block/sda/device/wwid")?
                    .trim()
                    .into(),
            ),
            Some(facts),
            None,
        )?;
        let pv_id = insert(
            &mut state.graph,
            NodeKind::LvmPv,
            Some(Bytes::new(pv.size)),
            ExternalId::LvmPvUuid(pv.uuid.clone()),
            None,
            None,
        )?;
        let vg_id = insert(
            &mut state.graph,
            NodeKind::LvmVg {
                name: vg.name.clone(),
                extent_size: Some(Bytes::new(vg.extent_size)),
            },
            Some(Bytes::new(vg.size)),
            ExternalId::LvmVgUuid(vg.uuid.clone()),
            None,
            None,
        )?;
        state.graph.insert_dependency(Dependency {
            from: disk,
            to: pv_id,
            kind: DependencyKind::Backs,
        });
        state.graph.insert_dependency(Dependency {
            from: pv_id,
            to: vg_id,
            kind: DependencyKind::MemberOf(MemberRole::LvmPv),
        });
        state.lvm_vgs.push(LvmVgObservation {
            node: vg_id,
            vg: vg.clone(),
        });
        let signatures = self.blkid.probe_blkid_signatures()?;
        let mut filesystem_devices = Vec::new();
        for lv in native.lvs {
            if lv.vg_name != vg.name || lv.segment_type.as_deref() != Some("linear") {
                return Err("unsupported private LV".into());
            }
            let active = lv.activation();
            let dm_uuid = format!(
                "LVM-{}{}",
                vg.uuid.replace('-', ""),
                lv.uuid.replace('-', "")
            );
            let mut endpoint = None;
            for entry in fs::read_dir("/sys/class/block")? {
                let entry = entry?;
                if fs::read_to_string(entry.path().join("dm/uuid"))
                    .is_ok_and(|uuid| uuid.trim() == dm_uuid)
                {
                    if endpoint.is_some() {
                        return Err("ambiguous DM UUID".into());
                    }
                    endpoint = Some(block(entry.file_name().to_str().ok_or("DM name")?)?);
                }
            }
            if active == Some(true)
                && endpoint
                    .as_ref()
                    .is_none_or(|(size, _)| size.as_u64() != lv.size)
            {
                return Err("active LV endpoint missing or wrong size".into());
            }
            let facts = endpoint.map(|(_, facts)| facts);
            let lv_id = insert(
                &mut state.graph,
                NodeKind::LvmLv(LvmLv {
                    name: lv.name,
                    kind: LvmLvKind::Linear,
                }),
                Some(Bytes::new(lv.size)),
                ExternalId::LvmLvUuid(lv.uuid),
                facts.clone(),
                active,
            )?;
            state.graph.insert_dependency(Dependency {
                from: vg_id,
                to: lv_id,
                kind: DependencyKind::Provides,
            });
            if let Some(facts) = facts {
                use std::os::unix::fs::MetadataExt;
                let signatures: Vec<_> = signatures
                    .iter()
                    .filter(|signature| {
                        fs::metadata(&signature.device).is_ok_and(|metadata| {
                            rustix::fs::major(metadata.rdev()) == facts.devno.major
                                && rustix::fs::minor(metadata.rdev()) == facts.devno.minor
                        })
                    })
                    .collect();
                if signatures.len() > 1 {
                    return Err("ambiguous filesystem signature".into());
                }
                if let Some(signature) = signatures.first() {
                    if signature.signature_type.as_deref() != Some("ext4") {
                        return Err("unexpected LV content".into());
                    }
                    let fs_id = insert(
                        &mut state.graph,
                        NodeKind::Filesystem {
                            kind: FilesystemKind::Ext4,
                            label: signature.label.clone(),
                        },
                        None,
                        ExternalId::Filesystem {
                            fs_type: "ext4".into(),
                            value: signature.uuid.clone().ok_or("FS UUID")?,
                        },
                        None,
                        None,
                    )?;
                    filesystem_devices.push((facts.devno.clone(), fs_id));
                    state.graph.insert_dependency(Dependency {
                        from: lv_id,
                        to: fs_id,
                        kind: DependencyKind::Backs,
                    });
                }
            }
        }
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
        for mount in self.mount.probe_libmount()? {
            let rows: Vec<_> = mountinfo
                .lines()
                .map(|line| line.split_whitespace().collect::<Vec<_>>())
                .filter(|fields| {
                    fields
                        .get(4)
                        .is_some_and(|target| Path::new(target) == mount.target)
                })
                .collect();
            let [row] = rows.as_slice() else {
                return Err("ambiguous mount target".into());
            };
            let (major, minor) = row
                .get(2)
                .ok_or("mountinfo device")?
                .split_once(':')
                .ok_or("mountinfo device")?;
            let devno = DeviceNumber {
                major: major.parse()?,
                minor: minor.parse()?,
            };
            if let Some((_, source)) = filesystem_devices
                .iter()
                .find(|(device, _)| *device == devno)
            {
                if mount.filesystem_type != "ext4" {
                    return Err("mount type mismatch".into());
                }
                state.mounts.push(ObservedMount {
                    source: MountSource::Filesystem(*source),
                    target: mount.target,
                    options: mount.options,
                    context: MountContext::Host,
                });
            }
        }
        Ok(state)
    }
}

impl StateProvider for GuestProbe<'_> {
    /// Names this restricted real-device experiment separately from production probe assembly.
    fn id(&self) -> &str {
        "c4-private-guest"
    }
    /// Covers only the one disposable guest disk; no host filesystem is shared into this VM.
    fn responsibility(&self) -> &str {
        "c4.private.disk"
    }
    /// This adapter combines native libraries with read-only kernel metadata.
    fn connection_kind(&self) -> ProviderConnectionKind {
        ProviderConnectionKind::BuiltIn
    }
    /// Converts any incomplete measurement to a blocking probe failure.
    fn probe(&self) -> Result<ProviderState, ProviderProbeError> {
        self.collect()
            .map_err(|error| ProviderProbeError::new("c4.probe", error.to_string()))
    }
}

/// Exclusion exists because this is a sole-process disposable VM without other devices or namespaces.
pub struct GuestPolicy<'a>(NativeStorageAdapter<'a>);
impl BackendCapabilityCheck for GuestPolicy<'_> {
    /// Actual installed native support remains mandatory.
    fn availability(
        &self,
        operation: &ProviderAction,
        observed: &LiveObservation,
    ) -> OperationAvailability {
        self.0.availability(operation, observed)
    }
    /// Only the guest process can mutate this private VG during the synchronous run.
    fn check_scope(
        &self,
        _: &ExecutionContext<'_>,
        _: &ResolvedAction,
        _: &LiveObservation,
    ) -> LiveCheckStatus {
        LiveCheckStatus::Satisfied
    }
    /// No service, helper daemon, shared disk or second mount namespace exists in this guest.
    fn check_consumer_exclusion(
        &self,
        _: &ExecutionContext<'_>,
        _: &ResolvedAction,
        _: &LiveObservation,
    ) -> LiveCheckStatus {
        LiveCheckStatus::Satisfied
    }
    /// The sole process owns the mount namespace and pins an empty target before authorizing mount.
    fn check_mount_exclusion(
        &self,
        _: &ExecutionContext<'_>,
        action: &ResolvedAction,
        _: &LiveObservation,
    ) -> LiveCheckStatus {
        let ProviderAction::Mount(disksgraph::host::MountAction::Mount { target, .. }) =
            action.operation()
        else {
            return LiveCheckStatus::Violated {
                reason: "wrong mount action".into(),
            };
        };
        match disksgraph::host::native_mount::open_mount_target(target) {
            Ok(_) => LiveCheckStatus::Satisfied,
            Err(error) => LiveCheckStatus::Violated {
                reason: error.to_string(),
            },
        }
    }
    /// The separate final native probe covers the same exclusive private disk.
    fn check_final_scope(&self, _: &ExecutionContext<'_>, _: &LiveObservation) -> LiveCheckStatus {
        LiveCheckStatus::Satisfied
    }
}

/// Exact plan reviewed by this VM-only caller before any runtime admission can pass.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewedPlan {
    /// First admitted attempt binds this explicit review; a different run cannot reuse it.
    pub run: Option<uuid::Uuid>,
    /// Digest computed from the already reviewed immutable plan, never learned from incoming requests.
    pub digest: String,
    /// Exact planned LV permitted to receive ext4.
    pub lv: NodeId,
}

/// Trusted caller supplies the reviewed scope separately from the plan and application wire data.
pub struct GuestApproval {
    /// None is fail-closed until the caller has reviewed and frozen this precise VM plan.
    reviewed: std::rc::Rc<std::cell::RefCell<Option<ReviewedPlan>>>,
    /// Optional fixed guest-owned review file, supplied outside application requests after freezing.
    review_path: Option<&'static str>,
}
impl AuthorizationContext for GuestApproval {
    /// Checks exact run/document and the fixed route for each supported action.
    fn check(&mut self, request: &ApprovalRequest) -> LiveCheckStatus {
        let backend = if request.action_id == ActionId::new(2) {
            "vm-ext4"
        } else if request.action_id == ActionId::new(3) {
            "vm-mount"
        } else {
            "vm-lvm"
        };
        let mut reviewed = self.reviewed.borrow_mut();
        if let Some(path) = self.review_path.filter(|_| reviewed.is_none()) {
            *reviewed = fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        }
        let Some(reviewed) = reviewed.as_mut() else {
            return LiveCheckStatus::Unknown {
                required_evidence: "explicit reviewed VM plan".into(),
            };
        };
        if reviewed.run.is_none_or(|run| request.run_id == run)
            && request.plan_digest == reviewed.digest
            && request.backend.as_str() == backend
            && request
                .targets
                .iter()
                .any(|target| matches!(target.external_ids.as_slice(), [ExternalId::LvmVgUuid(_)]))
        {
            reviewed.run = Some(request.run_id);
            LiveCheckStatus::Satisfied
        } else {
            LiveCheckStatus::Violated {
                reason: "guest approval mismatch".into(),
            }
        }
    }
    /// Authorizes formatting only the verified new LV fingerprint of this exact plan.
    fn check_destructive(&mut self, request: &ApprovalRequest, target: NodeId) -> LiveCheckStatus {
        let reviewed_lv = self.reviewed.borrow().as_ref().map(|review| review.lv);
        if Some(target) == reviewed_lv
            && request.action_id == ActionId::new(2)
            && request.targets.iter().any(|fingerprint| {
                fingerprint.planned_id == target
                    && matches!(
                        fingerprint.external_ids.as_slice(),
                        [ExternalId::LvmLvUuid(_)]
                    )
            })
        {
            self.check(request)
        } else {
            LiveCheckStatus::Violated {
                reason: "guest destructive target mismatch".into(),
            }
        }
    }
}

/// Observes the actual arm boundary and optionally injects one controlled target change after admission.
pub struct GuestDispatch<'a> {
    /// Same registrations used for readiness and admission.
    native: NativeStorageAdapter<'a>,
    /// Explicit guest-only fault; never changes production policies or adapters.
    fail_mount: bool,
    /// Counts native adapter entries to prove the failed attempt cannot be retried.
    calls: std::rc::Rc<std::cell::Cell<usize>>,
}
impl BackendDispatcher for GuestDispatch<'_> {
    /// Keeps the explicit primary registration.
    fn backend_id(&self) -> &BackendId {
        self.native.backend_id()
    }
    /// Uses the operation's exact configured native route.
    fn backend_for(&self, operation: &ProviderAction) -> &BackendId {
        self.native.backend_for(operation)
    }
    /// Requires a parseable durable checkpoint and held lock before entering any native adapter.
    fn execute(
        &mut self,
        action: &ResolvedAction,
        progress: &mut dyn FnMut(String) -> Result<(), ExecutionRunError>,
    ) -> Result<ExecutionReceipt, DispatchError> {
        let check = || -> Result<(), Box<dyn Error>> {
            let marker =
                ActiveAttempt::from_bytes(&fs::read("/run/c4-execution/active-attempt.json")?)?;
            if marker.run_id != action.run_id()
                || marker.action_id != action.action_id()
                || marker.action != *action.operation()
                || marker.targets != action.fingerprints()
                || marker.backend_id != *action.backend()
            {
                return Err("checkpoint differs from native invocation".into());
            }
            if ExecutionRoot::open("/run/c4-execution")?
                .try_lock_host()
                .is_ok()
            {
                return Err("host lock missing during dispatch".into());
            }
            if self.fail_mount && action.filesystem_target().is_some() {
                fs::write("/mnt/c4/injected-target-change", b"guest fault after arm")?;
            }
            Ok(())
        };
        check().map_err(|error| DispatchError::Adapter(error.to_string()))?;
        self.calls.set(self.calls.get() + 1);
        self.native.execute(action, progress)
    }
}

/// The same scoped native host is used by direct Rust, CLI and the separately built VM library.
pub type GuestApplication<'a> =
    DisksGraph<LinuxLiveHost<'a, GuestPolicy<'a>, GuestApproval>, GuestDispatch<'a>>;

/// Keeps native registrations alive through the caller's entire workflow or transport serve loop.
/// Root creation and fixture setup remain explicit guest caller actions after guard().
pub fn with_host(
    fail_mount: bool,
    reviewed: std::rc::Rc<std::cell::RefCell<Option<ReviewedPlan>>>,
    review_path: Option<&'static str>,
    serve: impl FnOnce(
        GuestApplication<'_>,
        std::rc::Rc<std::cell::Cell<usize>>,
    ) -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    guard()?;
    let registry = registry()?;
    let lvm = registry
        .backends()
        .find(|backend| backend.manifest().id.as_str() == "vm-lvm")
        .ok_or("LVM registration")?;
    let ext4 = registry
        .backends()
        .find(|backend| backend.manifest().id.as_str() == "vm-ext4")
        .ok_or("ext4 registration")?;
    let blkid = registry
        .backends()
        .find(|backend| backend.manifest().id.as_str() == "vm-blkid")
        .ok_or("blkid registration")?;
    println!("C4_STAGE=native_probe");
    let mount = registry
        .backends()
        .find(|backend| backend.manifest().id.as_str() == "vm-mount")
        .ok_or("mount registration")?;
    let provider = GuestProbe { lvm, blkid, mount };
    let probe = LinuxProbe::from_providers(vec![Box::new(provider)]);
    println!("C4_STAGE=native_readiness");
    let native = NativeStorageAdapter::new(lvm, ext4, mount)?;
    let approval = GuestApproval {
        reviewed: reviewed.clone(),
        review_path,
    };
    let host = LinuxLiveHost::new(probe, GuestPolicy(native), approval);
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let dispatcher = GuestDispatch {
        native,
        fail_mount,
        calls: calls.clone(),
    };
    let app = DisksGraph::new("/run/c4-execution", host, dispatcher)?;
    serve(app, calls)
}
