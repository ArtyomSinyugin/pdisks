# Recorded synthetic coordinator checkpoint

`coordinator-active-attempt.json` was captured after the real coordinator verified
its first CreateLv action, before durable clear, using `coordinator_crash_child`
with `DISKSGRAPH_COORDINATOR_PHASE=verified_before_clear`. The adapter changes only
a recorded in-memory graph; `fake-lvm` is not a native backend. UUIDs and the plan
digest are historical diagnostic data, never application handles or authorization.
Public recovery tests reuse these exact bytes to test new-owner refusal and close
preservation. This fixture does not establish native write/crash parity of transports.
