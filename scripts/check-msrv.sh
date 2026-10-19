#!/bin/sh
# Run the locked offline workspace and real cdylib consumer on exactly the declared minimum compiler.
# Usage: scripts/check-msrv.sh /path/to/rust-1.88.0/bin
set -eu
if [ "$#" -ne 1 ]; then
    echo "usage: $0 /path/to/rust-1.88.0/bin" >&2
    exit 2
fi
msrv_bin=$(cd "$1" && pwd)
case "$("$msrv_bin/rustc" --version)" in
    'rustc 1.88.0 '*) ;;
    *) echo 'MSRV verification requires rustc 1.88.0' >&2; exit 2 ;;
esac
case "$("$msrv_bin/cargo" --version)" in
    'cargo 1.88.0 '*) ;;
    *) echo 'MSRV verification requires cargo 1.88.0' >&2; exit 2 ;;
esac
repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_dir"
"$msrv_bin/rustc" --version
"$msrv_bin/cargo" --version
# Isolate compiler artifacts; tests still load the actual newly built cdylib beside their executable.
RUSTC="$msrv_bin/rustc" CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$repo_dir/target/msrv-1.88.0" \
    "$msrv_bin/cargo" test --workspace --all-targets --locked --offline
# The shipped client is a separate Cargo workspace with no path dependency on the application.
RUSTC="$msrv_bin/rustc" CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$repo_dir/target/msrv-1.88.0" \
    "$msrv_bin/cargo" build --manifest-path packaging/consumer/Cargo.toml --locked --offline
