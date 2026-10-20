#!/bin/sh
# Build a local Linux release bundle in a new directory; this does not install or publish it.
# Usage: scripts/stage-package.sh /path/to/new-bundle
set -eu
if [ "$#" -ne 1 ]; then
    echo "usage: $0 /path/to/new-bundle" >&2
    exit 2
fi
if [ -n "${CARGO_BUILD_TARGET:-}" ]; then
    echo 'This staging command builds for the host; unset CARGO_BUILD_TARGET.' >&2
    exit 2
fi
# Refuse existing files, directories and symlinks before any build or copy.
mkdir -m 0700 -- "$1"
bundle_dir=$(cd "$1" && pwd)
repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_dir"
"${CARGO:-cargo}" build --release --locked --offline -p dg-cli -p disksgraph-ffi
build_dir=${CARGO_TARGET_DIR:-"$repo_dir/target"}/release
install -Dm755 "$build_dir/dg-cli" "$bundle_dir/bin/dg-cli"
install -Dm755 "$build_dir/dg-backend4" "$bundle_dir/bin/dg-backend4"
# Keep the distinct Rust crate name; only the staged dynamic-library filename is the public name.
install -Dm755 "$build_dir/libdisksgraph_ffi.so" "$bundle_dir/lib/libdisksgraph.so"
install -Dm644 disksgraph-ffi/include/disksgraph.h "$bundle_dir/include/disksgraph.h"
install -Dm644 LICENSE "$bundle_dir/share/doc/disksgraph/LICENSE"
install -Dm644 Cargo.lock "$bundle_dir/share/doc/disksgraph/Cargo.lock"
install -Dm644 disksgraph-ffi/README.md "$bundle_dir/share/doc/disksgraph/ABI.md"
install -Dm644 docs/embedding.md "$bundle_dir/share/doc/disksgraph/EMBEDDING.md"
install -Dm644 docs/backend4.md "$bundle_dir/share/doc/disksgraph/BACKEND4.md"
install -Dm644 docs/host-trust.md "$bundle_dir/share/doc/disksgraph/HOST-TRUST.md"
install -Dm644 packaging/README.md "$bundle_dir/README.md"
for source in Cargo.toml Cargo.lock src/main.rs src/api.rs python/disksgraph.py python/check.py; do
    install -Dm644 "packaging/consumer/$source" "$bundle_dir/consumer/$source"
done
{
    "${RUSTC:-rustc}" --version --verbose
    "${CARGO:-cargo}" --version
    git rev-parse HEAD
} > "$bundle_dir/BUILD-INFO"
# Hash only shipped regular files, with relative names usable after moving the bundle.
cd "$bundle_dir"
find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort | xargs sha256sum > SHA256SUMS
chmod 0755 "$bundle_dir"
printf '%s\n' "$bundle_dir"
