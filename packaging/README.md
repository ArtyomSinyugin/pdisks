# Local Linux bundle

Build with `scripts/stage-package.sh /path/to/new-bundle` from the source checkout.
The destination must not exist. The command builds the locked release CLI and ABI
offline, without installing files or selecting native write policies. A failed
command may leave a partial directory; only a successful exit identifies a bundle.

The bundle contains `bin/dg-cli`, `lib/libdisksgraph.so`, protocol documentation,
GPL license, dependency lockfile, compiler/build information, a standalone checked
consumer SDK under `consumer/`, and SHA-256 checksums.
Verify it after moving with `cd /path/to/bundle && sha256sum -c SHA256SUMS`.
Cargo's build artifact remains `libdisksgraph_ffi.so` to keep its Rust crate distinct
from the facade crate `disksgraph`; the staged ABI filename is `libdisksgraph.so`.
VM examples and their write-capable fixture library are never staged.

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
inode are checked before and after flock acquisition, but this is not protection
against a privileged writer replacing them later. A completed run leaves only
that lock in an initially empty root. An active, malformed or unreadable
`active-attempt.json` blocks new execution. Process death before publication can
leave an inert `.active-attempt-*.tmp`; it grants no permission to dispatch.
There is no automatic checkpoint retry, replay or deletion during close. Use
incident review; acknowledgment still requires an independently configured
trusted principal/scope/quiescence policy and fails closed by default.

The executable never elevates privileges. Effective UID, directory ownership,
a provider manifest and a saved plan are not execution authorization. Production
privilege separation, service principals, ALT package ownership/lifecycle,
redaction and power-loss acceptance remain unfinished. No service unit,
privileged helper, setuid bit or system install is supplied by this bundle.
Treat raw observations, progress and backend diagnostics as potentially sensitive:
redaction is not implemented, so restrict access to captured output and documents.

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
cargo test -p storage-runtime --test execution_leases --offline
cargo test -p dg-cli --test exit_codes --offline
```

The ignored child helpers are invoked by their normal parent tests; do not run
all ignored tests manually. Fault switches exist only in unit-test builds.
SIGKILL phase tests use synthetic engine reports and no native dispatch. They
verify process/filesystem behavior, not power loss or public-route crash parity.
