# Local Linux bundle

Build with `scripts/stage-package.sh /path/to/new-bundle` from the source checkout.
The destination must not exist. The command builds the locked release CLI and ABI
offline, without installing files or selecting native write policies. A failed
command may leave a partial directory; only a successful exit identifies a bundle.

The bundle contains `bin/dg-cli`, `lib/libdisksgraph.so`, protocol documentation,
GPL license, dependency lockfile, compiler/build information and SHA-256 checksums.
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
