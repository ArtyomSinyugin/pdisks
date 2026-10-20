# Embedding disksgraph from Rust, Stabby, C and Python

The embedding application owns authorization, confirmation and environmental
exclusion. Runtime always owns native dispatch, checkpoints, correlation,
prerequisites and independent verification. Imported JSON cannot install policy.
The supported write recipe remains linear LV creation/activation, ext4 creation
on that verified new LV, and Host mount of its verified ext4 output. Other planning
operations do not acquire a write implementation through a positive policy result.

Rust applications can continue configuring `DisksGraph::new` with their own host.
For the supplied native Linux stack, construct `native::HostPolicy::new(policy)`
and pass its clone to `native::open_with_policy(root, manifest, policy)`. The
registry is retained by the application, with no borrowed-library lifetime for
the caller. Required registrations are `lvm`, `filesystems`, `libmount-mounts`
with explicit execute capabilities, plus the existing full native read stack.
Use the retained policy for incident acknowledgment if that workflow is supported.

Stabby clients use the additional checked export:

```rust,ignore
extern "C" fn disksgraph_open_with_policy(
    config: StableString,
    callback: extern "C" fn(u64, StableString) -> StableString,
    cookie: u64,
) -> StableString;
```

The ordinary `disksgraph_open` remains read-only. Both constructors use the same
configuration shape and owner/jobs/close lifecycle; no request method can toggle
an existing context into a write host. The callback runs synchronously on the
owner thread. Keep its code and cookie state alive until close has returned.
Never unwind across it, unload its code, or recursively wait on its own context.
Reentrant calls can encounter Busy and are not an authorization technique.

Callback JSON is `{policy_version:1,request:...}`. Request variants are:

- `check:"scope"`: `kind` is scope, consumer_exclusion, mount_exclusion,
  final_scope or preservation. `context` includes the reviewed actions,
  dependencies, baseline, goal and protected scope; `observed` is fresh evidence.
  `action` is present for action checks and null for final checks.
- `check:"authorize"`: the complete action/run/plan/digest/backend/target/
  observation request. `destructive_target:null` is general authorization;
  a separate callback with a target UUID requires consent to replace its contents.
- `check:"incident"`: immutable reviewed and fresh inspections. Explicit token
  confirmation alone cannot establish native quiescence after interruption.

Stabby replies are `{ "policy_version":1, "status":"satisfied" }`, or a typed
status such as `{ "policy_version":1, "status":{"unknown":{"required_evidence":"..."}} }`.
Malformed, oversized or unsupported responses become Unknown. Native support and
ordinary live checks remain mandatory even when the application returns Satisfied.
Policy input is limited to 64 MiB and Stabby replies to 64 KiB.

For applications not using Rust/Stabby types, the same `.so` additionally exports
the C JSON memory bridge in `include/disksgraph.h`. It uses explicit UTF-8 byte
lengths and library-owned NUL-terminated replies. Free every reply, including
errors, exactly once with `disksgraph_json_free`; never use the caller's allocator.
The C callback borrows versioned JSON for that one call and returns 1 (satisfied),
0 (violated), or -1/other (unknown). It must not retain the input pointer.
C bridge version is 1, separate from application JSON and Stabby signatures.
The bridge delegates to the same owner transport, not a parallel disk executor.

The bundled `consumer/python/disksgraph.py` uses Python 3 stdlib ctypes and JSON;
no Rust compiler is required. For a saved draft:

```python
from disksgraph import Library
api = Library("/usr/lib64/libdisksgraph.so")
context = api.open("/existing/private/root")
try:
    session = api.call(context, "session_import", draft=draft)["session"]
    preview = api.call(context, "session_preview", session=session)
finally:
    api.close(context)
```

Pass `manifest=...` and `policy=callable` to open a native host. The callable takes
`(cookie, versioned_request)` and returns an explicit integer decision; exceptions
and invalid return values are Unknown. Python retains live library/callback owners
until explicit successful close. The application must still close every context;
this retention prevents unsafe garbage collection, not resource leaks from
forgotten close. Do not infer authority from a root process or a successful load.
