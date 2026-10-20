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

For native writes the trusted startup configuration explicitly sets `"write":true`
and supplies an execute-enabled `manifest`. The process must run as root. This
selects the same root application policy as dg-cli, without per-user ACL inside
the executable. Ordinary config with no write flag remains read-only.

When a nonempty apply reaches authorization, stdout first emits a versioned reply
whose result has `type:"plan_review"` and the same review fields as the root CLI.
The frontend must respond on the same connection with a separate authorized frame:

```json
{"action":{"write":{"command":"apply","run_id":"<review.run_id>","plan_id":"<review.plan_id>","digest":"<review.digest>","token":"<review.token>"}}}
```

A read frame cannot confirm writes. Cancellation, EOF, wrong IDs/token or invalid
input refuses admission. The already-pending apply then emits its normal final
result. No other command is processed while consent is pending; do not send a
second apply in place of confirmation. Input buffering is shared with this exchange
so prefetched lines cannot disappear and there is no recursive stdin lock.
The same exclusive-storage administration assumption as [root CLI](root-cli.md)
applies. Altera still owns authorization of each outer write message.

The executable uses the existing synchronous application protocol: owner
job methods return unsupported-operation, while blocking apply uses the ordinary
coordinator. A backend4 write wrapper alone does not authorize disk mutations: root host
configuration, native capability, live safety checks and exact-plan confirmation
remain required.
