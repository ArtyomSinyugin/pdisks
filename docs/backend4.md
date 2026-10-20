# Rust Altera backend4 module

`dg-backend4` uses the same Rust facade, models and JSON application methods as
the Stabby library. It is a native executable, with no shell adapter and no
backend3 translation. It preserves sessions, drafts and retained runs until EOF.

The integration point is `altr-server/src/routes.rs`, the `backend4` branch around
line 139. Build altr-server with its `backend4` feature. Its stdio launcher executes
`ALTERATOR_LIBDIR/backend4/<module>` without arguments. Install `dg-backend4` there
as `disksgraph` to select module `disksgraph`. The Altera client selects backend4
and this module through its existing connection/switch API. No installer UI change
or disk-preview enablement is included.

At startup the executable reads `/etc/disksgraph/backend4.json`. Tests/deployments
can select another trusted startup file with `--config FILE` or environment
`DISKSGRAPH_BACKEND4_CONFIG`. A minimal saved-input configuration is:

```json
{"root":"/var/lib/disksgraph/execution"}
```

The root must already exist with mode 0700 and the effective user's ownership.
An optional `manifest` selects actual native provider reads. Neither the root nor
manifest can be overridden by client messages. The server operator owns startup
configuration and Altera access policy.

Each input is one complete JSON line. The outer action is the standard backend4
object used by `examples/backend4/altera-config`. The nested payload is the
existing disksgraph application envelope:

```json
{"action":{"list":{}}}
{"action":{"read":{"protocol_version":1,"request":{"method":"probe","params":{}}}}}
{"action":{"write":{"protocol_version":1,"request":{"method":"session_create","params":{}}}}}
```

Replies are the existing `{protocol_version,result}` or `{protocol_version,error}`
envelopes, one line per request, flushed immediately. There is no startup banner
or extra final success record. `list` describes the module and supported outer
actions. Saved-draft import/edit/preview/freeze, capabilities, apply, run evidence,
rebase and incident methods use the same facade as other transports.

`read` permits probe, capabilities, previews/exports, run snapshots/events, job
poll and incident inspection/review. All other methods require `write`; new methods
default to write until explicitly classified. Altera authorizes the outer action
before forwarding. The adapter independently rejects mismatched methods, multiple
action keys, unknown envelope fields and unknown application versions.

Malformed bounded requests return an error and leave the connection usable. An
oversized frame receives a resource-limit reply and closes the stream without
interpreting its remaining bytes. EOF releases process-owned objects and never
clears an incident checkpoint. Broken stdout ends with an I/O failure.

The current executable uses the existing synchronous application protocol: owner
job methods return unsupported-operation, while blocking apply uses the ordinary
coordinator. Native policy/confirmation support is tracked separately in the
[product contract](product-contract.md); a backend4 write wrapper alone does not
authorize disk mutations. Default native construction currently disables writes.
