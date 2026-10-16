# Providers, backends, and connections

DisksGraph treats three independently evolving concepts as separate contracts:

| Concept | Examples | Responsibility |
|---|---|---|
| Logical provider | `luks`, `lvm`, `partition`, `filesystem` | Object semantics, typed actions, constraints, validation and translation into the DisksGraph model |
| Technology backend | `libcryptsetup`, `libfdisk`, `libbd_lvm` | Concrete calls to the operating system and native APIs |
| Provider connection | built-in Rust, `.so` through Stabby, separate process | Delivery of DisksGraph provider requests and responses |

For example, the built-in `LuksProvider` owns `LuksAction::{Format, Open,
Close, Resize}`, validates those actions against `NodeGraph`, and translates
cryptsetup records into `LuksObservation`. Its `CryptsetupBackend` is currently
implemented by a registered `libcryptsetup` backend.

There are two unrelated ABI boundaries:

```text
DisksGraph host     <-> DisksGraph provider plugin   future project-owned Stabby ABI
DisksGraph provider <-> libcryptsetup            existing upstream C ABI
```

Stabby must not wrap or redefine libcryptsetup. It will only connect the host
to a separately built logical provider. A provider action remains, for example,
`LuksAction::Open` regardless of whether the provider is built in, loaded as a
DisksGraph plugin, or hosted in another process.

## Provider actions and contracts

`ProviderAction` is a closed serializable enum dispatching typed partition,
LUKS, LVM, filesystem, Btrfs, and mount actions. Actions contain only domain
parameters and planned node identities. They never contain backend references,
library paths, function pointers, closures, or arbitrary command arguments.

Creating actions name their model outputs explicitly through
`planned_node_id`. Values generated only by a backend, including future UUIDs,
device numbers, and capacity after provider overhead, remain unknown until
postcondition probing.

`describe_action` derives one `ActionContract` from the typed action and the
trusted graph context. It contains prerequisites, provided outputs, resource
access, the shared predicted effect, invalidated facts, postconditions, and a
restart policy. `storage-engine` validates actions through `validate_provider_action`
and interprets this `predicted_effect` against the preceding projected state.

`simulate_plan_for_goal` (and its capacity-evidence variant) returns a
`SimulationReport`: the ordered prerequisite trace, final goal checks, typed
intent coverage with action IDs, and every unresolved `Unknown` obligation.
A matching final graph cannot erase a destructive approval, minimum-size check,
or unread persistent configuration. Coverage describes proposed operations;
it does not report that `Check` or `Repair` actually ran. Replaying against a
goal with an absent intent index rejects inconsistent action provenance.

`is_complete` requires satisfied goal checks, covered intents, and no unknown
prerequisites. Known goal mismatches remain visible in the report and make it
incomplete; goal-aware composition still rejects them. Completion concerns
planning evidence only. Every action still requires live admission and verified
postconditions before execution can be considered successful.

Goal comparison keeps node kinds, their properties, and all graph relationships
exact. Capacity has one explicit rule: requested `NodeSpec.size == None` leaves
that field unmanaged. An explicit requested size with unknown projected capacity
produces `Unknown`; a known wrong capacity produces `Violated`. Unknown size
never conceals a different kind, relationship, or another node's known mismatch.
Other optional fields retain their existing exact semantics.

`compose_plan_for_goal_with_policy` and `simulate_plan_for_goal_with_policy`
accept an explicit `ProtectedScope`. They examine each trusted step, so restoring
a label later cannot conceal an earlier write. Protected nodes include incident
topology, metadata resource writes, and mutation-related invalidations propagated
from parent activation. Read-only checks may refresh evidence. Runtime targets
protect ancestors and descendants in the same context; persistent protection
conservatively covers configuration writes anywhere in that context because the
current contract writes a whole configuration resource. Paths follow the same
absolute, non-traversing lexical rules as mount actions; live path resolution is
still required. Runtime and persistent protection remain separate.

Reports retain every action/aspect conflict and become incomplete. Policy-aware
composition rejects a conflict. Existing APIs use an empty protection policy;
callers must supply objects to preserve explicitly. The policy concerns proposed
mutations and does not replace destructive approval or live identity checks.

The JSON contract is informational. After importing a plan,
`verify_imported_contract` recalculates the complete contract and compares its
canonical semantics with the imported representation. On success it returns the
recalculated instance. Editing JSON therefore cannot remove `NoUnexpectedConsumers`, reduce
resource locks, or weaken postcondition verification.

`storage-engine::PlanDocument` is the versioned JSON boundary. It records a
plan identity, the original goal, protection policy, saved `HistoricalBaseline`,
capacity evidence, actions, and dependencies. `PLAN_SCHEMA_VERSION` describes
the outer format; `ACTION_SEMANTICS_VERSION` describes the shared provider
catalogue independently of provider transport and backend-manifest versions.
Plan schema 3 uses separate runtime and persistent mount lists. Action semantics 4
removes the redundant `MountEntry.persistent` flag: `MountAction::Persist` and
`MountEffect::SetPersistent` select persistent configuration themselves. Stale
entry flags are rejected during deserialization.
Action semantics 5 requires typed persistent mount sources to exist and have a
compatible node kind, with read access coordinated against source mutations.
Persistence alone does not require the source to be active or mounted.
There are no run statuses, library handles, authorizations, or credential contents.

`capture` validates the description before saving. `from_json` checks the document
schema version before typed parsing;
`analyse_historical` validates IDs, fragment provenance, references, graph edges,
and the DAG before replay. It reuses the same predecessor-state interpreter and
imported-contract verifier, retains trusted recalculated contracts, then checks
resource coordination, goal coverage, and protected objects. Unknown obligations
remain conditional; known violations reject import. `analyse_against` additionally
compares the saved context with an independently retained historical baseline.
This conservative comparison is for historical analysis, not live correlation.

`canonical_json` validates and produces stable object keys, graph edge sets,
action IDs, dependency sets, mount entries, capacity evidence, and protection
sets. Contract prerequisites, outputs, resource accesses, invalidations, and
postconditions are sets. Consecutive edge removals commute inside the atomic
batch and are normalized as a run; other graph effects, mount effects, allocation
effects, and ordered intents retain sequence. Duplicate edge removals retain
multiplicity and cannot match a trusted contract that removes an edge once.

`HistoricalAnalysis` explicitly represents saved evidence. Plan and baseline
UUIDs identify documents; they do not authenticate content or authorize writes.
Fresh identity correlation, per-action admission, and actual postcondition
verification remain separate execution work. JSON rejects non-UTF-8 paths while
identical in-process contracts can still be verified without JSON encoding.

`ExecutionRun` freezes the historically validated plan and assigns a separate
run UUID. Its serial lifecycle is `Waiting → Checking → Running → Verifying →
Verified`; failure or unknown outcome blocks ordinary continuation. `Running`
records an admitted invocation attempt, not a verified model effect. There is no
automatic retry, skip-by-final-shape shortcut, or deserializer that can inject
runtime states. Recovery must observe actual state and enforce the frozen
`RestartPolicy` through a separate trusted policy.

`LiveExecutionChecks` is the future runtime host contract. Admission must attest
fresh identity/scope/capability applicability, every frozen prerequisite, and
all required resource leases. Verification must cover every frozen postcondition
against the retained post-backend publication. A generic successful checker or
`NodeReprobed` cannot replace `FilesystemCheckPassed`. Missing, duplicate, stale,
or cross-run reports are rejected. A known violation stops the action; unknown
admission stays in `Checking`, and unknown postconditions stop in `OutcomeUnknown`.

Actual publications carry a probe-instance UUID and monotonic `ProbeEpoch`.
Admission, completion, and final verification require newer publications from
the same instance; action verification references its exact retained completion
snapshot. Failed or partial backend outcomes retain the supplied actual snapshot.
If a post-backend probe is unavailable, `mark_outcome_unknown` stops the attempt
while retaining the last actual snapshot. No predicted effect is applied to it.

Final actual-goal and protected-area verification remains separate even after
all actions reach `Verified`. Run events carry the run UUID and a monotonically
increasing sequence for progress and reconnect replay; progress never verifies
an effect. Reports are attestations from a trusted host, not authorization tokens
or a substitute for real probing. Native implementations, actual lease ownership,
identity binding adjacent to native calls, and write dispatch remain disabled.

The current filesystem resize recipe supports ext2/ext3/ext4. Shrink requires
unmounting the filesystem in every observed namespace, then a forced read-only
`Check` whose `FilesystemCheckPassed` postcondition verifies no errors were found.
`NodeReprobed` alone does not establish that result. The simulator projects this
check output conditionally on successful verification; it never reports an actual
checker result. A non-forced check or `Repair` does not supply the guarantee.
Mounting, repairing, resizing, or changing backing geometry revokes it.

The shrink target also requires provider evidence of a safe minimum size;
simulation retains that condition as `Unknown`. Unknown current size requires
the conservative shrink preparation. Other filesystem resize recipes are rejected
until their constraints are implemented. ext2 growth also requires unmounting.
These constraints follow the upstream [resize2fs documentation](https://github.com/tytso/e2fsprogs/blob/master/resize/resize2fs.8.in)
and the [e2fsck read-only and mounted-state rules](https://github.com/tytso/e2fsprogs/blob/master/e2fsck/e2fsck.8.in).

`FilesystemAction::SetLabel` writes the direct backing endpoint and requires its
`Writable` prerequisite, just like filesystem resize and repair. Deactivated or
read-only backing rejects the transition before changing the projected label;
unknown write access remains a live obligation. The shared backing write resource
also requires ordering with activation and other backing operations. This does
not establish filesystem-specific online-label support; live applicability still
belongs to the trusted provider checker.

Mount goals compare the opaque option sequences exactly, including order and
repetition: `ro,rw` and `rw,ro` must not prove the same requested behavior.
Runtime and persistent goals share this rule. JSON canonicalization preserves
these sequences. Provider-specific option normalization is not implemented,
so even reordered independent options require an exact match during planning;
this conservative comparison never guesses equivalence from a string set.

Graph-backed runtime mounts require source availability in the predecessor state.
Ordinary filesystems need one direct block backing: an explicit deactivation is
rejected, a fresh endpoint observation or predicted interface permits access,
and missing or invalidated observations remain `Unknown`. Read-only endpoints
can supply read access; mount-option permissions and filesystem health still
need live checks. Btrfs, subvolumes, datasets, and ambiguous backing topology
retain a provider availability obligation. Source and ancestor node read locks
coordinate mounting with backing changes and group deactivation. Persistent
entries do not require a currently active source. Action semantics version 3
rejects historical JSON plans using earlier contracts, including mount availability
and filesystem-label backing checks.

Runtime mount changes and offline filesystem actions share the conservative
`RuntimeMountState` resource, so a safe sequential preview cannot authorize an
unordered DAG. Bind and unsupported mount sources require further live evidence
to prove that the filesystem is unmounted. Real write dispatch is still disabled.

External requirements are checked against the consumer's trusted predecessor
trace. A preceding unmount, filesystem shrink, or capacity release can establish
a condition absent from the original snapshot. Trusted unknown conditions remain
live obligations; declaring them externally neither proves them nor discards them.

Named outputs are bound in the consumer's intermediate predecessor context,
including writable activation and forced clean checks of newly created filesystems.
Adding a dependency restarts the pure trace from C0, so creating actions and allocation
are not applied twice to an existing projection. Already replayed actions cannot
be retried implicitly to restore a revoked guarantee. An explicit multi-phase
recipe uses its latest ordered output provider; unordered alternatives remain
ambiguous. Outputs explicitly ordered after a consumer cannot satisfy its requirement.
Every declaration must occur in the trusted action contract. Final replay and
shared-resource checks still reject unsafe transitions and incidental ordering;
effects without named outputs require an explicit transition recipe.

Nested runtime targets receive `MountOrder` dependencies before the first
simulation: parents are mounted before new children, and children are unmounted
before changing their parents. Existing explicit multi-phase recipes retain their
order and are replayed for safety. `MountDescendantsAbsent` prevents a parent
change from hiding or detaching a still-mounted child, including in direct effect
application. Declared descendant requirements may be established by preceding
unmounts in that trace, rather than requiring absence in the original snapshot.

Ancestry compares path components within the same `MountContext`, so `/data2`
is not below `/data`, and distinct namespaces or target roots are not merged.
Mount targets, bind sources, and target roots must be absolute and contain no
parent traversal or NUL. Persistent entries follow the same path rules but do
not acquire runtime mount-order edges. This is lexical planning; symbolic links,
directory existence, and real namespace bindings still require live verification.
The [mount](https://man7.org/linux/man-pages/man8/mount.8.html) and
[umount](https://man7.org/linux/man-pages/man8/umount.8.html) manuals describe
the underlying mount hierarchy and namespace behavior.

Linear LVM allocation uses the owning VG's physical extents. `CreateLv` reserves
the requested extents, `ResizeLv` reserves or releases the difference from its
predecessor-state size, and `RemoveLv` releases that predecessor allocation once.
Known free capacity is supplied as baseline `CapacityEvidence`; it is never
inferred by subtracting logical node sizes from VG size. Allocation effects
remain atomic with graph changes, including overflow checks on returned extents.

Known linear sizes must be exact multiples of a nonzero extent size; requests
are rejected instead of silently rounded. Missing or ambiguous VG ownership is
also rejected. Missing sizes or extent geometry and non-linear layouts require
provider allocation calculation. Their `AllocationEffect::Invalidate` discards
the domain's old free-capacity forecast, so later allocations remain conditional
until fresh provider evidence is available. Logical thin or RAID sizes are not
treated as physical allocation. The [LVM creation manual](https://man7.org/linux/man-pages/man8/lvcreate.8.html)
describes physical extent allocation and virtual sizes.

Simulation keeps a cumulative invalidation ledger alongside the graph and lists
invalidated observation aspects on each action step. Historical `NodeFacts` stay
unchanged inside `SimulationResult.desired.graph`; consumers check
`SimulationResult.evidence.is_observation_stale` for the relevant scope and
`FactKind`. A new prediction is not a new measurement. Preserved access
observations retain `ObservedSnapshot` provenance even after a size prediction.

Activation changes revoke prior endpoint claims, including dependent nodes such
as LVs in a VG. Stale access observations require fresh evidence; explicit later
preparation can supply a new predicted interface guarantee. Effects, clean-check
revocation, and observation invalidations publish atomically, so failed actions
leave all previous evidence intact. Automatic LUKS resize uses the shared
`InvalidateNodeSize` effect: subsequent capacity checks cannot use the old size
and remain `Unknown` until a new size is established. A fresh `CurrentState`
starts a new projection with its own observations.

## Backend manifests

Backend manifests belong to disksgraph. Third-party packages such as `libzfs`,
LVM, and libfdisk do not contain or maintain them.

Each manifest describes one system backend integration and the native symbols
or command it needs. `manifest_version` versions this JSON schema only. Native
API requirements differ between integrations and are expressed by
`library.required_symbols` plus backend-specific adapter tests. The JSON field
is still named `providers` for schema-version-1 compatibility; Rust code exposes
these records as `BackendManifest` through `BackendRegistry`.

At runtime disksgraph reads one manifest file and registers capabilities only when
the declared library loads with every required symbol, or when the declared
command exists. A missing optional package is therefore normal and does not
prevent disksgraph from starting.

The file is `/usr/share/disksgraph/providers.json` by default. Set
`DISKSGRAPH_PROVIDER_MANIFEST` to replace it.

The disksgraph RPM build should:

1. generate one architecture-specific manifest from a project-owned template,
   expanding `%_libdir` into an absolute library path;
2. install it as `%_datadir/disksgraph/providers.json`;
3. add runtime `Requires` selected by the package maintainer;
4. add matching `BuildRequires` when native integration tests are enabled.

Third-party RPM specs need no changes.

Example generated manifest:

```json
{
  "manifest_version": 1,
  "providers": [
    {
      "id": "zfs",
      "library": {
        "path": "/usr/lib64/libzfs.so",
        "required_symbols": ["libzfs_init", "libzfs_fini"]
      },
      "capabilities": ["probe"]
    }
  ]
}
```

Set `DISKSGRAPH_PROVIDER_MANIFEST` when running tests to make
`installed_provider_manifests_match_host` verify every selected manifest
against the libraries and commands installed by `BuildRequires`.

RPM `%check` example:

```sh
DISKSGRAPH_PROVIDER_MANIFEST="$PWD/providers.json" \
  cargo test -p storage-provider --test installed_tools \
  installed_provider_manifests_match_host -- --ignored --exact
```

This strict test opens every declared library, resolves every required symbol,
and checks that every declared command is a regular executable file. Runtime
discovery remains permissive and skips unavailable optional providers.

The LVM probe checks GLib `GError` for PV/VG/LV queries before interpreting a
null result as an empty list. Missing, empty, or invalid UTF-8 required identities
fail the whole query instead of silently omitting records. Native arrays own all
records until parsing completes, so early errors also free unvisited entries.
Standalone LVM init/query/close sessions are serialized within the process; this
protects plugin lifecycle only and does not authorize storage writes.

## Backend ownership

`LinearLvRequest` validates the first native write recipe: a new linear LV in
an existing UUID-qualified VG. Its pure preflight requires a unique VG, an
unused LV name, sufficient free space and an exact extent-aligned size.
Constructing or checking a request does not authorize writes.
`RegisteredBackend::check_linear_lv_support()` additionally requires explicit
manifest `execute` capability, all read/create symbols, successful plugin init,
and libblockdev BASIC CREATE/QUERY support. It never supplies scope or approval
evidence; ordinary probe-only manifests remain unable to execute.

`RegisteredBackend::create_linear_lv()` is the explicit native mutation boundary.
It repeats technology checks and reads PV/VG/LV state in one serialized plugin
session before `bd_lvm_lvcreate`. Missing PVs, changed UUIDs, occupied names,
insufficient capacity and extent rounding abort before the call. No extra args
or caller-selected PV list are passed. A subsequent native query must establish
a new unique LV UUID with the requested owner, size and linear layout.
The runtime must retain its lease/checkpoint and independently probe/verify the
result. External LVM writers must also be excluded: libblockdev's name-based
create call cannot atomically bind the earlier UUID check. No automatic retry
or fallback is provided, and normal probe manifests do not opt into writes.
The disk-free ABI regression test builds a mock shared library with `cc` and
checks pre-call rejection and post-call ambiguity without loading system LVM.

One operation has one owning backend. A second library may enrich discovery,
but it must not become an alternative executor for the same action.

| Area | Owner | Supporting source |
|---|---|---|
| Ordinary filesystem operations | `libbd_fs` | `libblkid` discovers signatures |
| Btrfs operations and member topology | `libbd_btrfs` | `libbtrfsutil` reads subvolumes |
| Partition tables | `libfdisk` | `libblkid` may expose `PARTUUID` |
| LUKS | `libcryptsetup` | `libblkid` discovers the header |
| Device mapper | `libdevmapper` | Technology adapters interpret mapped content |
| ZFS | `libzfs` + `libzfs_core` + `libnvpair` | `libblkid` may discover member labels |
| NVMe and NVMe-oF | `libnvme` | udev supplies block endpoint facts |
| Runtime and persistent mounts | `libmount` | Filesystem providers supply source facts |

Consequently disksgraph does not plan parallel adapters for `libbd_part`,
`libbd_crypto`, `libbd_dm`, or `libbd_nvme` while their selected native owner
covers the required operation. The generic mount and Btrfs entrypoints in
`libbd_fs` are also not used: those responsibilities belong to `libmount` and
`libbd_btrfs` respectively.

`libbd_fs` presents a library API, but many calls intentionally execute a
filesystem-specific system utility. Its `bd_fs_can_*` API reports that runtime
dependency. disksgraph exposes the missing utility as an environment capability; it
does not create a second CLI fallback backend.

## Implemented coverage and TODO

`Probe` below means data is copied into typed Rust observations and, where
applicable, joined into `CurrentState`. `Execute` remains disabled until the
project has serializable actions, focused plan-generation tests, precondition
checks, and rollback semantics. The backend manifest declares only operations
that exist in the public Rust API today.

| Provider | Implemented now | Required next work |
|---|---|---|
| `libudev` | Block endpoints, aliases, device numbers, size, logical/physical and I/O geometry, alignment, vendor/model/serial/WWN, transport, read-only/removable/rotational/zoned flags | Monitor-based incremental updates, enclosure/controller failure domains, complete zoned limits |
| `libblkid` | Filesystem, swap, LUKS, LVM and MD signature identity used to connect graph layers | Low-level safe probing, multiple/ambivalent signatures, signature offsets and wipe evidence |
| `libfdisk` | Partition table and partition discovery | Free-range calculation API, damaged GPT evidence, type/attribute completeness, validated create/delete/resize/write plans |
| `libmount` | Runtime source, target, filesystem type and options | `fstab` parsing/writing, typed options, identity policy, pass number and ordering |
| `libdevmapper` | Mapping name/UUID/device number, target names, dependencies and read-only state | Target parameter/status parsing for cache, writecache, integrity, verity and multipath; validated create/remove/resize plans |
| `libcryptsetup` | LUKS version/UUID, cipher/mode, payload offset, sector/key/header area sizes and public keyslot states; payload capacity is reflected in `CurrentState` | PBKDF and token metadata, active mapping correlation, format/open/close/keyslot/resize/reencrypt plans |
| `libbd_fs` | Supported filesystem list, static features, resize modes and actual host availability including required utilities | Typed validation requests; mkfs/check/repair/resize/label/UUID actions after the common action safety layer exists |
| `libbd_lvm` | PV/VG/LV identity, sizes/free space, missing/exported state, usage/progress, physical segments, origin/pool relations and basic segment type | Complete thin/cache/RAID role graph and all validated mutation plans |
| `libbd_mdraid` | Active arrays, clean/degraded counts and diagnostics, status/bitmap, member superblock size/time/event freshness, and basic RAID layouts | Full RAID5/6/10 layout, member slot/role/state, resync progress and mutation plans |
| `libbd_mpath` | Member paths joined to device-mapper multipath mappings | WWID identity, path state/priority, health and early path deduplication before content probing |
| `libbd_loop` | Backing file, offset and runtime flags | Setup/remove/resize actions |
| `libbd_swap` | Activation state | mkswap/swapon/swapoff/label/UUID actions with error-vs-inactive distinction |
| `libbd_btrfs` | Filesystem UUID/label/usage, member paths/sizes/usage/devid and operation-group availability | Allocation profiles and balance state; typed mkfs/member/subvolume/snapshot/resize/check/scrub actions |
| `libbtrfsutil` | Mounted subvolume ID/path, default and read-only flags | Subvolume UUID/parent/snapshot identity and operation progress where available |
| OpenZFS libraries | Imported pools, GUIDs, full vdev tree, mirror/RAIDZ/dRAID layouts, allocation classes, leaf paths, missing/faulted/degraded state, dataset mountpoint/compression/quota/zvol size/origin | Importable pools, snapshots/clones and typed pool/vdev/dataset/zvol/import/export/scrub actions |
| `libnvme` | Subsystem/controller/namespace topology, transport/address/service, controller model/serial/firmware/state, NSID, LBA format/count/utilization and native identifiers | ANA/path state, zoned namespace facts and typed connect/disconnect/format/sanitize actions |

The table is deliberately stricter than ABI availability. Resolving a symbol
does not count as implementing the corresponding backend operation.

## Planned native integrations

These integrations are not yet present in the runtime manifest:

| Area | Selected API | Boundary |
|---|---|---|
| iSCSI | `libopeniscsiusr` | Discovery and session data first; use a typed `iscsiadm` adapter only for operations absent from the library |
| Ceph RBD | `librados` + `librbd` | Cluster/pool/image discovery and map/unmap lifecycle |
| NBD | `libnbd` | Export discovery and connect/disconnect lifecycle |
| EFI | `libefiboot` + `libefivar` | Build EFI load options/device paths and read/write firmware variables; bootloader file installation remains a separate system integration |
| DRBD | Not selected | No stable DRBD library/header is available on the current build host. Compare generic-netlink support with a versioned `drbdsetup` machine-readable adapter before implementation |
| bcache | sysfs for probe; executor not selected | No `libbd_bcache` or other stable bcache library is available on the current build host. Evaluate a typed `make-bcache` adapter for creation |

Network block lifecycle needs additions to `DesiredState` before these providers
can plan connections without hiding endpoint, authentication, and reconnect
policy in strings. EFI execution similarly waits for the shared action and
privilege boundary.

## Backend contract gap

The target contract in `docs/storage-backend-spec.md` contains
`probe_environment`, `scan`, `capabilities`, alignment/overhead queries,
`validate`, create/destroy/modify planning, `execute`, and `rollback`.

At present this crate implements native availability checks and read-only probe
adapters plus the `libbd_fs`/Btrfs capability queries described above. Automatic native planning, execution, and rollback remain TODOs; pure fragment
composition and simulation live in `storage-engine`. None is implied by loading
a native library.
