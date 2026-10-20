# Trusted host configuration

`disksgraph::native::open` configures native reads and a disabled write dispatcher.
Its `root` and `manifest` arguments are trusted host inputs. JSON protocol callers
must not be allowed to select them on behalf of a more privileged process. The
CLI accepts them from the process invoker; the Stabby constructor accepts them
from its embedding process. Neither entry point is a privilege boundary.

The native constructor checks matching real/effective UID and GID before opening
the root or manifest. It rejects mismatched credentials even in saved-only mode.
An ordinary root invocation is allowed, but supplies no execution authorization.
This check does not detect saved IDs, supplementary groups, file/ambient
capabilities, namespaces, or prior privilege changes. It does not drop privileges
or make installation with setuid/setgid/file capabilities supported. The trusted
embedding host must keep process credentials stable throughout every call and
must not expose these APIs as a privileged service without its own authenticated
boundary.

The execution root is opened and validated before native libraries are loaded;
that same descriptor is retained by the application. The root must already have
mode 0700 and belong to the effective user. Its parents must be trusted and stable;
ancestor ownership, symlinks and mount changes are not validated. Every cooperating
process must select the same root. No configuration check excludes external LVM,
filesystem or mount writers.

A backend manifest is executable configuration: `dlopen` may execute constructors
before any probe method runs. A read-only storage policy cannot sandbox native
code. The host must trust the manifest, all selected libraries, auxiliary libraries,
executables, their parent directories, transitive loader dependencies and loader
environment. Absolute paths alone do not prove this trust. Registry APIs do not
validate package signatures, directory ownership or replacement races. The
`DISKSGRAPH_PROVIDER_MANIFEST` override in `BackendRegistry::load` is also trusted
configuration; the native facade uses the explicitly supplied path instead.

Before the first library load, the registry reads at most 1 MiB plus one rejection
byte, validates the collection version, all backend paths/symbols and uniqueness
of every backend ID, including unavailable optional backends. NUL paths are
rejected. Optional missing dependencies are skipped only after this validation.
A later declaration's structural error cannot follow a successful library load. Valid native
libraries can still fail initialization after executing their constructors.

A custom Rust host supplied through `DisksGraph::new` remains a trusted extension.
The host owns provider selection, complete scope and consumer/namespace exclusion,
principal identity and authorization, backend routing, and final preservation
checks. That constructor does not validate how the host acquired its privileges
or native registrations. Imported drafts/plans cannot replace its policies.

A positive `AuthorizationContext` must independently establish the real caller's
authority and any required confirmation for the entire `ApprovalRequest`: run,
plan, action, canonical plan digest, backend, all target fingerprints and fresh
observation. It must check again for each admitted action. The request is context,
not a credential. Root, a valid plan, a manifest `execute` capability and a held
flock cannot supply approval. Destructive filesystem creation requires the separate
`check_destructive` result as well. The ordinary read-only constructor keeps policies Unknown and its dispatcher
disabled. Explicit `open_with_policy` uses the embedding application policy and
the same native adapters; root CLI/backend4 supply their own confirmation policy.

Incident acknowledgment has a separate trusted policy. A matching challenge only
confirms review; it does not authenticate the caller or prove native backend
quiescence. Authorization, complete scope and stopped native operations must all
be established before clearing the exact active attempt. Close, reopen and root
privileges do not acknowledge an incident.

[Embedding](embedding.md) defines the native policy callback; [root CLI](root-cli.md)
defines the standalone application's authority/confirmation and exclusion contract.
A separate user permission system or privileged service is not required for these
products. Native crash/power-loss acceptance and system installation remain distinct
acceptance work; they must not reopen already-proved synthetic gates. Full storage
observation anonymization is a separate feature, not an implicit library guarantee.
The disks preview stays disabled.
