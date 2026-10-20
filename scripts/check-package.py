#!/usr/bin/env python3
"""Check staged files, production exports and actual ABI/CLI preview parity without native writes.

Usage: scripts/check-package.py BUNDLE CONSUMER_EXECUTABLE
The consumer may be built by a different supported Rust compiler than the library.
"""

import hashlib
import json
import pathlib
import subprocess
import sys
import tempfile


def run(*args):
    """Execute explicit argv; stderr remains visible while JSON stdout is captured."""
    return subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE).stdout


def verify(bundle):
    """Reject changed/missing/extra files and paths escaping the bundle before loading its library."""
    expected = set()
    for line in (bundle / "SHA256SUMS").read_text().splitlines():
        digest, relative = line.split(maxsplit=1)
        path = bundle / relative
        if not path.resolve().is_relative_to(bundle) or path.is_symlink():
            raise ValueError(f"unsafe checksum path: {relative}")
        if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError(f"checksum mismatch: {relative}")
        expected.add(path.resolve())
    actual = {path.resolve() for path in bundle.rglob("*") if path.is_file() and path.name != "SHA256SUMS"}
    if actual != expected:
        raise ValueError("bundle file set differs from SHA256SUMS")
    symbols = run("nm", "-D", "--defined-only", str(bundle / "lib/libdisksgraph.so"))
    names = {line.split()[-1] for line in symbols.splitlines() if line.split()}
    for name in ["api_version", "open", "open_with_policy", "call", "close", "json_api_version", "json_open", "json_open_with_policy", "json_call", "json_close", "json_free"]:
        if "disksgraph_" + name not in names:
            raise ValueError(f"missing export: {name}")
    if any(name.startswith("disksgraph_vm_") for name in names):
        raise ValueError("bundle contains a VM acceptance artifact")


def main():
    """Use only a new private synthetic root; preview and close must preserve its marker bytes."""
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    bundle, consumer = (pathlib.Path(arg).resolve(strict=True) for arg in sys.argv[1:])
    verify(bundle)
    repo = pathlib.Path(__file__).resolve().parents[1]
    recorded = json.loads((repo / "storage-engine/examples/fixtures/probe_snapshot.json").read_text())
    draft = {
        "schema_version": 5, "session_id": "00000000-0000-7000-8000-000000000005", "revision": 0,
        "baseline": {"id": "00000000-0000-7000-8000-000000000006", "snapshot": recorded["current"], "capacity_evidence": []},
        "draft": {"operations": [], "protected_scope": {"nodes": [], "runtime_mounts": [], "persistent_mounts": []}},
    }
    with tempfile.TemporaryDirectory(prefix="disksgraph-package-") as temp:
        root = pathlib.Path(temp)
        source = root / "draft.json"
        source.write_text(json.dumps(draft))
        marker = root / "active-attempt.json"
        marker.write_bytes(b"retained synthetic incident")
        marker.chmod(0o600)
        abi = json.loads(run(str(consumer), str(bundle / "lib/libdisksgraph.so"), str(root), str(source)))
        cli = json.loads(run(str(bundle / "bin/dg-cli"), "plan", "preview", "--root", str(root), "--draft", str(source)))
        if abi != cli:
            raise ValueError("ABI/CLI preview differs")
        python_check = run(sys.executable, str(bundle / "consumer/python/check.py"),
                           str(bundle / "lib/libdisksgraph.so"), str(source))
        if "PYTHON_ABI_VERIFIED" not in python_check:
            raise ValueError("non-Rust Python consumer did not complete")
        if marker.read_bytes() != b"retained synthetic incident":
            raise ValueError("preview/close changed the incident")
    print("PACKAGE_VERIFIED: checksums, production exports, checked ABI/CLI preview, Python C client, retained marker")


if __name__ == "__main__":
    main()
