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
