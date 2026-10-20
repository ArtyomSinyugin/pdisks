#!/usr/bin/env python3
"""Exercise the actual .so from Python without Rust or any native storage writes.

Usage: check.py LIBRARY DRAFT_JSON
"""

import json
import pathlib
import sys
import tempfile
sys.dont_write_bytecode = True
from disksgraph import Library


def main():
    """Round-trip a saved draft and compare ABI preview with its retained historical baseline."""
    library = Library(pathlib.Path(sys.argv[1]).resolve(strict=True))
    draft = json.loads(pathlib.Path(sys.argv[2]).read_text())
    with tempfile.TemporaryDirectory(prefix="disksgraph-python-") as directory:
        root = pathlib.Path(directory)
        marker = root / "active-attempt.json"
        marker.write_bytes(b"retained incident evidence")
        marker.chmod(0o600)
        context = library.open(root)
        try:
            imported = library.call(context, "session_import", draft=draft)
            session = imported["session"]
            preview = library.call(context, "session_preview", session=session)
            exported = library.call(context, "session_export", session=session)
            assert exported["baseline"] == draft["baseline"]
            assert preview["revision"] == draft["revision"]
            try:
                library.call(context, "SECRET-private-method")
                raise AssertionError("unknown method accepted")
            except RuntimeError as error:
                assert "invalid_request" in str(error)
                assert "SECRET" not in str(error)
            library.call(context, "session_close", session=session)
        finally:
            library.close(context)
        assert marker.read_bytes() == b"retained incident evidence"
    print("PYTHON_ABI_VERIFIED: actual library, owned buffers, draft preview, retained marker")


if __name__ == "__main__":
    main()
