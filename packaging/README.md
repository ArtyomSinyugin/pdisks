# Local Linux bundle

Build with `scripts/stage-package.sh /path/to/new-bundle` from the source checkout.
The destination must not exist. The command builds the locked release CLI and ABI
offline, without installing files or selecting native write policies. A failed
command may leave a partial directory; only a successful exit identifies a bundle.

The bundle contains `bin/dg-cli`, `bin/dg-backend4`, `lib/libdisksgraph.so`, protocol documentation,
GPL license, dependency lockfile, compiler/build information, a standalone checked
consumer SDK under `consumer/`, and SHA-256 checksums.
Verify it after moving with `cd /path/to/bundle && sha256sum -c SHA256SUMS`.
Cargo's build artifact remains `libdisksgraph_ffi.so` to keep its Rust crate distinct
from the facade crate `disksgraph`; the staged ABI filename is `libdisksgraph.so`.
VM examples and their write-capable fixture library are never staged. Backend4
installation and framing are described in `share/doc/disksgraph/BACKEND4.md`.

This is a native host build, not a cross-platform distribution or system package.
It uses host loader/runtime libraries; the native provider libraries and their
trusted manifest are configured separately. There is no permissive write or
incident acknowledgment policy in the default CLI/ABI constructor. Existing
private execution directories must be provided explicitly; this bundle creates
no global execution root, service, history or configuration under `/etc`.

Use the Stabby 72.1.16 checked-import interface described in
`share/doc/disksgraph/ABI.md`. It passes stable owned strings; it is not a plain C
string interface. Close all contexts and free library-owned strings before
unloading. Source links in ABI.md refer to the original source checkout. Retain
that checkout to use the Rust facade; source archives and system packages are
separate release work. This staging command does not publish a release.

The SDK has its own Cargo workspace and lockfile, pinned Stabby 72.1.16 and
libloading 0.9.0, and no path dependency on the application. From the bundle:

```sh
CARGO_TARGET_DIR=/tmp/disksgraph-client cargo build --manifest-path consumer/Cargo.toml --locked --offline
/tmp/disksgraph-client/debug/disksgraph-consumer-example "$PWD/lib/libdisksgraph.so" /existing/private/root /path/to/draft.json
```

The command prints the saved draft preview; it configures no native host or writes.
Use an external target directory so compilation does not change the checksummed
bundle. Offline builds require the locked dependencies to be available in Cargo's
cache. The SDK's checked loader is also used by integration and VM consumers.

From the source checkout, verify a built client against the bundle with
`scripts/check-package.py /path/to/bundle /path/to/disksgraph-consumer-example`.
This checks hashes/file set, production exports, ABI/CLI preview parity and marker
preservation on a new private fixture root. It invokes no native writes. Both Rust
1.88.0 and 1.98.1 producers and consumers were verified, in all four combinations;
other targets/toolchains need their own acceptance. SHA256SUMS provides local
integrity checking, not a signed release or an authenticity mechanism.

## Execution directory and process contract

All cooperating Rust, CLI and ABI hosts must explicitly select the same local
execution directory, owned by their effective user with exactly mode `0700` and
no special permission bits. Missing roots are rejected; the application neither
creates a root nor repairs its permissions. An opened but subsequently removed
root is rejected on lock acquisition. Renaming a retained root does not redirect
its descriptor: the operator must keep its original directory and permanent
`execution.lock` inode stable for the entire lifetime of all hosts.

Use a durable location for write-capable host integration so an active incident
survives reboot. `/var/lib/disksgraph/execution` is a proposed ALT deployment
location, not an installed default or a completed distribution policy. `/run` and
private temporary directories are suitable for disposable acceptance fixtures;
they do not establish reboot persistence. The parent path must be trusted and
stable: only the final component is opened with `NOFOLLOW`, so writable parents
or parent symlinks are not validated by this API. The filesystem must support
advisory flock, no-replace rename and directory fsync; failure has no weaker
fallback. A container or another UID selecting a different root does not share
this exclusion. External disk tools do not participate in it.

Do not delete or recreate `execution.lock` to resolve contention. Its name and
inode are checked before and after flock acquisition and again during checkpoint
operations. Lock/checkpoint files reject executable and special permission bits.
These checks cannot exclude a privileged writer replacing them later. A completed run leaves only
that lock in an initially empty root. An active, malformed or unreadable
`active-attempt.json` blocks new execution. Process death before publication can
leave an inert `.active-attempt-*.tmp`; it grants no permission to dispatch.
There is no automatic checkpoint retry, replay or deletion during close. Use
incident review; acknowledgment still requires an independently configured
trusted principal/scope/quiescence policy and fails closed by default.

The native constructor checks matching real/effective UID and GID before root
or manifest access. Mismatched credentials fail even in saved-only mode; ordinary
root execution supplies no authorization. Do not install with setuid/setgid bits
or file capabilities. The embedding process owns its credentials, environment and
all configuration paths; this API is not a privileged service boundary.

Backend manifests can load native code during open. They, their backend paths,
loader dependencies and parent directories must be trusted. Structural manifest
validation is complete before the first load, with a 1 MiB input bound and regular
file requirement. The native constructor retains the root descriptor validated
before loading. The full [host trust contract](share/doc/disksgraph/HOST-TRUST.md)
is shipped with this bundle. Rust consumers matching error enums exhaustively must
handle the new `BackendRegistryError::InvalidBackendPath` and
`CheckpointError::Lease` variants; JSON/ABI versions and signatures are unchanged.

The executable never elevates privileges. Effective UID, directory ownership,
a provider manifest and a saved plan are not execution authorization. Production
privilege separation, service principals, ALT package ownership/lifecycle,
full sensitive-data export policy and power-loss acceptance remain unfinished. No service unit,
privileged helper, setuid bit or system install is supplied by this bundle.
Public opaque diagnostic text is now omitted as described below. Raw observations,
storage identities, saved documents and trusted-host logs remain sensitive; restrict
access to captured output and documents.

CLI process codes are `0` for command success (apply requires final verification),
`1` for host/configuration/application/output failure, `2` for usage or invalid
input documents, and `3` for a returned unverified apply run. Invalid saved drafts
and plans now use `2`; their structured application codes are retained. Event
output failure takes priority and uses `1`, including after verified execution.
Embedders use the same `Error::exit_code()` and `result_exit_code()` helpers.

Focused source-checkout acceptance commands:

```sh
cargo test -p storage-runtime --lib checkpoint::store::tests --offline
cargo test -p storage-runtime --test checkpoint crash_lifecycle --offline
cargo test -p storage-runtime --test runtime coordinator_crash --offline
cargo test -p disksgraph --test recovery --offline
cargo test -p dg-cli --test recovery --offline
cargo test -p disksgraph-ffi --test recovery --test checked_consumer --offline
cargo test -p storage-runtime --test execution_leases --offline
cargo test -p dg-cli --test exit_codes --offline
```

The ignored child helpers are invoked by their normal parent tests; do not run
all ignored tests manually. Fault switches exist only in unit-test builds.
The coordinator SIGKILL matrix now covers armed, running (inside the synthetic
adapter), completed and verified-before-clear through the actual runtime path.
Restart is rejected before probe/admission/dispatch; observer unwinding retains
the stop latch. The earlier engine-only matrix remains a separate lower-level test.
A recorded marker from that coordinator is reused across facade/CLI/job recovery
tests; the checked .so also preserves it through saved-only reopen/preview/close.
Those public recovery tests do not kill native write-capable transport calls.
No native dispatch or power-loss acceptance is implied by this synthetic suite.

Rust embedders of `CheckpointStore` must pass `clear(&expected_attempt)` instead
of the former `(run_id, action_id)` arguments. Clear compares the complete armed
attempt, including digest, action, target fingerprints and backend; changed valid
JSON is preserved and blocks successors even after action verification. The
coordinator retains the original attempt through clear. Protocol/ABI versions
and checked signatures are unchanged.

Live snapshot consumers may now receive repeated event cursors at durable arm
and verified-before-clear boundaries; replace the whole snapshot. Neither an
Admitted armed action nor a Verified action before clear establishes final run
success. The events adapter still delivers each event once.

## Diagnostic serialization boundary

Opaque public error text and execution progress serialize as the fixed string
`[diagnostic redacted]`. This applies to facade/protocol/CLI/transport errors,
progress events (including JSONL and passive job polling), live-check/backend
outcome/readiness reasons, action failure reasons and CLI event-output errors.
The implementation suppresses complete messages rather than matching guessed
secret patterns. Stable error codes, states, IDs, sequence/cursor values,
verified/finished flags and optional-field presence are preserved. Code that
parsed diagnostic wording must switch to structured categories.

Raw Rust messages, Display/Debug and retained engine evidence are unchanged;
trusted hosts must keep them private. JSON events/status are display data and
cannot round-trip the original diagnostic text. No raw-JSON override is supplied.
Protocol versions and checked ABI signatures remain unchanged. This shared Serde
boundary is used by direct serialization, CLI, protocol and job/checked consumers.

This is not a complete privacy guarantee: probe/provider diagnostics embedded in
observations, preview/plan diagnostics, device paths/names/identities and exported
drafts/plans remain data. Neither native-library stderr nor panic hooks nor the
embedding host's own logs are intercepted. The saved-document/checkpoint contract
and canonical plan digest are not changed. Full sensitive-observation export and
production privilege/principal policies still require separate C8 acceptance.

Focused checks from the source checkout:

```sh
cargo test -p disksgraph --test diagnostics --offline
cargo test -p storage-engine diagnostic_serialization --offline
cargo test -p dg-cli --test diagnostics --offline
cargo test -p disksgraph-ffi --all-targets --offline
```
