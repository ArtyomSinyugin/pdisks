# Native recipe through Rust, CLI and checked Stabby imports

The experiment runs `CreateLv → SetActivation → CreateFilesystem(ext4) → Mount(Host)`
through `disksgraph::DisksGraph` and the real `ExecutionCoordinator`.
The Rust/CLI workflow lives in `dg-cli/examples/c6_vm.rs`; native registrations,
probe, policies and dispatcher are shared in `dg-cli/examples/support/guest.rs`.
They import neither `storage-engine` nor `storage-provider` directly. Probe, session creation, editing,
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
python3 storage-runtime/tests/vm/run_c4.py --rescue-iso /path/to/rescue.iso --interface ffi
python3 storage-runtime/tests/vm/run_c4.py --rescue-iso /path/to/rescue.iso --interface ffi --case mount-target-change
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

`--interface ffi` builds the separate `c7_vm` checked-import consumer and
`libc7_vm_library.so` acceptance artifact. The library's `disksgraph_vm_*` symbols
have the same owned-string signatures and Stabby reports as production exports.
Its fixed guarded host factory calls the ordinary `transport::Worker::serve`; it
adds no execution, polling, checkpoint or retry loop. Production `disksgraph_open`
still constructs a read-only host and has no VM/write-policy flag. A host-side
ignored integration test proves the VM factory refuses an unguarded machine.
The portable consumer also checks VM symbols are absent from the production cdylib.

The ABI workflow creates/edits/previews/freezes through the loaded library, then
checks saved-input preview and canonical plan parity against direct Rust, ignoring
only the newly allocated plan ID. The guest consumer explicitly supplies the
reviewed digest and destructive LV scope through fixed private `/run/c7-review.json`
after freezing; this trusted guest configuration is outside application requests.
It is read once by the same guest approval policy and bound to the first actual
run UUID. This file is not an available production approval channel.

ABI start/poll must expose an unfinished snapshot during real native execution.
Every exclusive event page is accumulated and compared with retained run events;
final snapshot must equal ordinary `run_snapshot`. Success checks four Verified,
fresh independent final evidence and a proof-file round trip on real ext4. Closing
the context leaves only `execution.lock` in its execution root.

ABI failure checks the same three Verified and Mount OutcomeUnknown as Rust/CLI.
Another apply fails on the marker before a fifth native entry. Inspection matches
its exact three-fingerprint checkpoint and exact-token acknowledgment is refused.
As with CLI, transient NodeIds may trigger Changed before default Unknown safety;
backend quiescence is not claimed. Close preserves the checkpoint. Reopening the
context loses historical run handles, retains the exact marker and refuses apply
without any native dispatch. The runner requires `C7_ABI_VERIFIED` and
`C7_LIVE_POLL_VERIFIED`, plus failure-specific `C7_INCIDENT_REFUSAL_VERIFIED` and
`C7_CLOSE_PRESERVATION_VERIFIED`. Four observed native adapter entries are required
in the original context, zero in the reopened one. Default production writes and
native incident acknowledgment remain unavailable.

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
