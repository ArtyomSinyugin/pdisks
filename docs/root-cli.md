# Standalone root CLI and confirmation

`dg-cli` is an application embedding the Rust library, not a separate disk engine.
Queries and saved-document commands keep their existing interfaces. `plan apply`
requires real/effective root IDs, a trusted native manifest and an existing private
execution root. There is no per-user ACL, helper daemon, automatic elevation or
blanket `--yes` permission.

The operator owns an exclusive storage administration interval: other disk tools,
source writers and mount/namespace mutations must be excluded throughout admission,
dispatch and verification. The plan review states this assumption explicitly.
This is the application's operating contract, not a fact inferred from a snapshot.
Native device fingerprints, writable endpoint guards and observed prerequisites
are still independently checked by the normal runtime.

Only the implemented linear-LV/ext4/Host-mount recipe is writable. The native
manifest must explicitly enable execute for `lvm`, `filesystems` and
`libmount-mounts` and supply the full read stack. The shipped live probe example
remains probe-only; selecting it does not silently authorize writes.

```sh
dg-cli plan apply --root /existing/private/root --manifest /trusted/providers.json \
  --file /reviewed/plan.json --events jsonl
```

After whole-plan readiness and the first action's live checks, stderr receives one
flushed JSON record with `type:"plan_review"`. Its `review` contains actions,
dependencies, historical baseline, requested goal, protection, actual run ID,
plan ID, digest and one-time token. Before any backend invocation, stdin must
receive one JSON line copied from that review:

```json
{"command":"apply","run_id":"<review.run_id>","plan_id":"<review.plan_id>","digest":"<review.digest>","token":"<review.token>"}
```

Unknown fields, wrong challenge/digest/IDs, EOF, malformed or oversized input all
refuse consent. `{"command":"cancel"}` also refuses. Consent covers only that
exact immutable run; later actions retain ordinary live checks and a separate
explicit destructive-policy invocation. A new explicit run requires a new review.
The CLI does not hold a second stdin lock while the confirmation callback waits.
Review and events remain separate from the final JSON on stdout.

No prompt is issued if earlier safety checks or native configuration already
reject the plan. A successful transport call is not verified execution: retain
existing exit/result conventions, including exit 3 for a returned unverified run.
No automatic retries or rollback are introduced. Incident acknowledgment still
needs independently established native quiescence; root and a correct token alone
cannot remove an unresolved checkpoint.
