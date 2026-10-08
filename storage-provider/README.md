# Providers, backends, and connections

PDisks treats three independently evolving concepts as separate contracts:

| Concept | Examples | Responsibility |
|---|---|---|
| Logical provider | `luks`, `lvm`, `partition`, `filesystem` | Object semantics, typed actions, constraints, validation and translation into the PDisks model |
| Technology backend | `libcryptsetup`, `libfdisk`, `libbd_lvm` | Concrete calls to the operating system and native APIs |
| Provider connection | built-in Rust, `.so` through Stabby, separate process | Delivery of PDisks provider requests and responses |

For example, the built-in `LuksProvider` owns `LuksAction::{Format, Open,
Close, Resize}`, validates those actions against `NodeGraph`, and translates
cryptsetup records into `LuksObservation`. Its `CryptsetupBackend` is currently
implemented by a registered `libcryptsetup` backend.

There are two unrelated ABI boundaries:

```text
PDisks host     <-> PDisks provider plugin   future project-owned Stabby ABI
PDisks provider <-> libcryptsetup            existing upstream C ABI
```

Stabby must not wrap or redefine libcryptsetup. It will only connect the host
to a separately built logical provider. A provider action remains, for example,
`LuksAction::Open` regardless of whether the provider is built in, loaded as a
PDisks plugin, or hosted in another process.

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
restart policy. Future simulation must consume this `predicted_effect` instead
of maintaining a second implementation of action semantics.

The JSON contract is informational. After importing a plan,
`verify_imported_contract` recalculates the complete contract and compares it
with the imported representation. On success it returns the recalculated
instance. Editing JSON therefore cannot remove `NoUnexpectedConsumers`, reduce
resource locks, or weaken postcondition verification.

## Backend manifests

Backend manifests belong to pdisks. Third-party packages such as `libzfs`,
LVM, and libfdisk do not contain or maintain them.

Each manifest describes one system backend integration and the native symbols
or command it needs. `manifest_version` versions this JSON schema only. Native
API requirements differ between integrations and are expressed by
`library.required_symbols` plus backend-specific adapter tests. The JSON field
is still named `providers` for schema-version-1 compatibility; Rust code exposes
these records as `BackendManifest` through `BackendRegistry`.

At runtime pdisks reads one manifest file and registers capabilities only when
the declared library loads with every required symbol, or when the declared
command exists. A missing optional package is therefore normal and does not
prevent pdisks from starting.

The file is `/usr/share/pdisks/providers.json` by default. Set
`PDISKS_PROVIDER_MANIFEST` to replace it.

The pdisks RPM build should:

1. generate one architecture-specific manifest from a project-owned template,
   expanding `%_libdir` into an absolute library path;
2. install it as `%_datadir/pdisks/providers.json`;
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

Set `PDISKS_PROVIDER_MANIFEST` when running tests to make
`installed_provider_manifests_match_host` verify every selected manifest
against the libraries and commands installed by `BuildRequires`.

RPM `%check` example:

```sh
PDISKS_PROVIDER_MANIFEST="$PWD/providers.json" \
  cargo test -p storage-provider --test installed_tools \
  installed_provider_manifests_match_host -- --ignored --exact
```

This strict test opens every declared library, resolves every required symbol,
and checks that every declared command is a regular executable file. Runtime
discovery remains permissive and skips unavailable optional providers.

## Backend ownership

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

Consequently pdisks does not plan parallel adapters for `libbd_part`,
`libbd_crypto`, `libbd_dm`, or `libbd_nvme` while their selected native owner
covers the required operation. The generic mount and Btrfs entrypoints in
`libbd_fs` are also not used: those responsibilities belong to `libmount` and
`libbd_btrfs` respectively.

`libbd_fs` presents a library API, but many calls intentionally execute a
filesystem-specific system utility. Its `bd_fs_can_*` API reports that runtime
dependency. pdisks exposes the missing utility as an environment capability; it
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
adapters plus the `libbd_fs`/Btrfs capability queries described above. Planning,
execution, and rollback are project TODOs, not implied by a loaded library.
