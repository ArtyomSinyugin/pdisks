# Native recipe through the Rust facade and C6 CLI command adapter

The experiment runs `CreateLv → SetActivation → CreateFilesystem(ext4) → Mount(Host)`
through `disksgraph::DisksGraph` and the real `ExecutionCoordinator`.
The shared guest example now lives in `dg-cli/examples/c6_vm.rs`; it imports neither
`storage-engine` nor `storage-provider` directly. Probe, session creation, editing,
preview, freeze, apply, events, snapshots and incident inspection use the facade. Each action requires a durable checkpoint
and held lock before entering the actual installed native adapter. Actual LVM,
blkid, libmount and sysfs observations supply verification; the separate final probe
must also pass. The success case writes and reads a file on the mounted filesystem.

The host runner creates a new 512 MiB **regular image** and a minimal initramfs,
then boots QEMU with TCG. It passes no host disks, network adapters or shared host
directories. Guest-only `pvcreate` and `vgcreate` prepare the fixture; all four
planned mutations use the normal runtime path. The guest refuses to start without
the special kernel command line, expected SCSI WWN, exact disk size, no additional
physical backing devices and only the loopback network interface. Empty kernel
ram/loop devices are allowed. No root privileges are required on the host.

On the current ALT Linux x86_64 setup, provide a trusted rescue ISO containing
`/boot/vmlinuz` and a gzip `/boot/initrd.img` with zstd kernel modules. Host tools:
Python 3, Cargo, QEMU, xorriso, cpio, zstd, gzip, file, strip, static busybox, LVM and
e2fsprogs. Native libraries are read from `/usr/lib64`: `libbd_lvm.so.3`,
`libbd_fs.so.3`, `libblkid.so.1`, `libmount.so.1`, plus their loader dependencies.
The script fails when these requirements are missing; it does not download them.

```sh
python3 storage-runtime/tests/vm/run_c4.py --rescue-iso /path/to/rescue.iso
python3 storage-runtime/tests/vm/run_c4.py --rescue-iso /path/to/rescue.iso --case mount-target-change
python3 storage-runtime/tests/vm/run_c4.py --rescue-iso /path/to/rescue.iso --interface cli
python3 storage-runtime/tests/vm/run_c4.py --rescue-iso /path/to/rescue.iso --interface cli --case mount-target-change
```

`--interface rust` (default) exercises the direct facade route. `--interface cli`
uses the real CLI argument parser and command adapter for preview/freeze/apply.
Both routes retain the same explicitly bounded guest probe, approval and exclusion
policies. CLI preview and executable plan inputs match the direct API; separate
freeze calls allocate distinct plan IDs. The JSON result matches the facade's
retained snapshot, events match its sequence, and imported plans have no fabricated
trusted source map. CLI success requires `C6_CLI_VERIFIED` and verified exit status.
The CLI failure case additionally retries through the command adapter (blocked
before any fifth dispatch), inspects the actual marker, and sends the exact token
from a same-process incident review. The executable's default Unknown acknowledgment
policy and runtime freshness checks cannot remove that marker. GuestProbe deliberately
regenerates transient NodeIds on each pass, so Changed may refuse before the safety
policy; the test does not weaken comparison or claim backend quiescence.

This tests the command adapter with trusted guest configuration; the ordinary
`dg-cli` executable still has no permissive production write policy or VM bypass flag.

Each command creates its own directory under Python's temporary directory and
prints its location. The image, serial log and generated initramfs are retained
there for inspection; the runner's exit status requires the appropriate guest
assertions and success sentinel, since a clean QEMU exit alone proves nothing.

`mount-target-change` inserts one file in the target **after** Mount's checkpoint
is armed. Native endpoint validation then fails. The guest requires the first
three actions to remain Verified, Mount to become OutcomeUnknown, its checkpoint
to retain all three native fingerprints, and a new facade apply attempt to stop
on that marker without another native dispatch. The original run remains inspectable. No fault injection exists in production
adapters or policies.

The guest supplies the exact reviewed plan digest and destructive LV scope through
separate host-owned state after freezing. JSON never contains an approval flag;
the first admitted request binds this review to its actual run UUID. Snapshot
assertions check action-to-operation source maps, bounded event cursors, and final
publication freshness independently of native success. The runner also requires
`C5_FACADE_VERIFIED`, in addition to the existing C4 outcome sentinel.

The VM policy attests exclusion only for this sole-process, disposable environment;
it is not a production policy. General scope and `check_mount_exclusion()` still
default to Unknown. Only this Host ext4 recipe and explicit option profile are
covered. No unmount, persistent configuration, concurrent external writers,
power-loss recovery or other library/kernel combinations are claimed.

The real integration run exposed a `move_mount(..., "/proc/self/fd/N", ...) = EINVAL`
with libmount 2.39.2. Passing the retained directory as `/proc/self/fd/N/` resolves
the directory before the non-following final target lookup. A syscall trace then
confirmed successful fsopen/fsconfig/fsmount/mount_setattr/move_mount, and both
completion and independent final verification passed. The private C ABI fixture
also checks this trailing slash; it never performs a real mount.
