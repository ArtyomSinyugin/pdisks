# Product boundary and completion criteria

The product has three entry points over the same engine and runtime:

1. The Rust library and separately packaged Stabby dynamic library are the core
   product. Embedding applications own authorization and confirmation policy.
   The public dynamic library must expose host policy configuration without
   requiring the caller to rebuild a Rust wrapper or a VM-specific library.
2. `dg-cli` is a standalone embedding application. It runs as root, has no
   per-user permission system, and requires confirmation of the exact reviewed
   plan. Root authority does not replace device correlation, prerequisites,
   checkpoint durability or independent result verification.
3. `dg-backend4` is a Rust executable for Altera backend4. It reuses the same
   library/application logic and exchanges newline-delimited JSON over stdin and
   stdout. The reference transport is altr-server's `src/routes.rs` backend4
   branch and `examples/backend4/altera-config`, not backend3 shell processing.

Backend4 receives `{"action":{"read":{...}}}` or
`{"action":{"write":{...}}}`. Altera authorizes this outer action before forwarding
it. The adapter must reject storage-changing methods wrapped in `read`, retain
one application's sessions for the connection, and flush one JSON reply per input.
Configuration paths come from process startup, never from forwarded requests.
The existing installer disk page and its disabled preview are outside this work.

Completion requires usable core Rust/ABI host configuration, standalone CLI
confirmation, backend4 parity, and packaging/documentation for these interfaces.
Acceptance uses the existing recorded fixtures and guarded disposable native VM.
A safety invariant discovered to be false blocks completion; adding permutations
of an already proved invariant is not a new milestone. There is no target number
of commits: 20 commits is only the user's mandatory pause interval.

Per-user access control inside root CLI, a privileged daemon, universal external
writer exclusion, automatic crash retry, additional storage recipes and a GUI are
not part of these entry-point requirements. Host exclusion assumptions must be
explicit and cannot be silently represented as observed facts. Checkpoints after
unknown native outcomes remain blocking until separately reviewed.

C8 is not complete merely because its tests pass: the shipped read-only ABI lacks
external host policy configuration, and ordinary CLI apply currently lacks a
positive confirmation route. These are concrete remaining functionality, together
with backend4 integration, rather than reasons to keep expanding synthetic gates.
