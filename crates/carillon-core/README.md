# Durable core

[`Core`](src/engine.rs) owns admission, conversation/runtime ownership, execution outcomes and delivery operations over real SQLite. It has no HTTP request types, credentials, bundled provider choices, or process startup. [`examples/conversation.rs`](examples/conversation.rs) is the shortest complete caller:

```sh
mise run example:core
```

The example creates temporary storage, accepts and deduplicates a message, invokes an explicitly deterministic in-process runtime, reads the captured resource snapshot, and observes/acknowledges delivery. No Discord or model account is used. The example is also executed by `cargo test`.

## Construct and use

- `Core::create(CoreOptions)` exclusively creates fresh storage (private directory and 0600 database); it refuses existing files, symlinks, and non-private existing data directories rather than adopting or changing them. A failed initialization may leave a reserved file for the caller to inspect/remove.  `Core::open(CoreOptions)` requires an existing compatible database. Options name the agent, resource root, default runtime and concrete runtime descriptors. The application supplies these facts; core never chooses Pi or Discord.
- `accept(interface, event).await` returns a durable receipt. **The caller must authenticate external ingress before supplying the interface identity.** Native in-process callers have the same Unix authority as any other code in that process; this API is not a second permission system. An event with `attachments` names a batch the interface staged in its own staging directory of the installation (`carillon_installation::config::attachment_staging`); core captures those files before it accepts the event, and the capture continues if the caller stops waiting. `CoreOptions::attachments` bounds counts, sizes, storage and concurrent captures.
- `event_status(interface, event_id)` reports an event as accepted (with its receipt), still being captured, or unknown, for interfaces settling a lost answer.
- `runtime_for(interface_id, query)` resolves the conversation owner for model operations; existing conversations do not follow a changed default.
- `run_next(runtime_id, lane, operation)` claims one eligible turn of a lane (`Lane::Messages` or `Lane::Observations`), calls the supplied runtime operation, validates its answer and commits completion plus delivery intent atomically. A message's request also carries, as `unanswered`, earlier files of the conversation that no completed turn has carried yet, which the runtime may never have recorded; a completed turn marks what it carried: its own files, and the earlier ones unless its runtime answers that it left them (`unanswered_left`), as T3's runtime does for a command it sends as written. An observation's request carries the runtime's cursor as `follow_up`. It is one step, not an implicit background worker; `work(lane)` is a `watch` receiver that changes when this handle may have made a turn of the lane claimable, so a worker that found nothing waits instead of polling. Verify local transport prerequisites before claiming. [`RuntimeClient`](src/runtime_client.rs) is the service's concrete authenticated HTTP collaborator; an in-process caller can supply an ordinary async function instead.
- `claim_delivery` (or `claim_turn_delivery` for one turn's), `acknowledge_delivery` and `reconcile_deliveries` preserve interface-instance ownership and delivery uncertainty. They never infer that ambiguous delivery is safe to repeat.
- `active_turns(interface)` lists an authenticated interface's queued and running turns with their owning runtime. The service relays each running turn's runtime snapshot at `GET /v1/activity`; core stores no activity.
- `recover_interrupted` and `reconcile_attachments` are explicit. Call them only when taking exclusive ownership after the old worker has stopped and before accepting events: reconciliation removes capture copies no committed turn references. Opening another handle or running diagnostics is not a restart. `Core` runs no workers of its own: the application owns workers, cancellation and the process lock. It starts only work a call asks for: attachment captures, which run as tasks of the caller's Tokio runtime until their copy returns (`stop_capturing` asks running captures to stop at their next chunk during shutdown), and the blocking storage work of async calls described below, which is tracked until it ends.
- Storage operations are synchronous. Async paths (`accept`, `run_next`, and the service's delivery handlers through `blocking`) run SQLite writes, which wait up to five seconds for another writer, and resource snapshots on Tokio's blocking pool and await them, so a waiting write never occupies an async worker; plain reads stay inline, since in WAL mode a reader never waits for the writer. Such work finishes even if its caller stops waiting, with the same outcome a synchronous call would have; `storage_settled` waits for it, and the service calls it at shutdown before releasing its lock.

`TurnFailure::Retry` means the runtime **provably did not receive** the work. Anything ambiguous is `Uncertain`, terminally interrupting that turn and leaving the conversation usable for later input. `Explained` is a failure the runtime reported with an explanation for the conversation. Core stores it as the turn's error and includes it in the interruption notice, with the same no-replay semantics. A failed database outcome write remains running until explicit startup recovery. There is no alternate lifecycle store, generic repository, or transcript reconstruction.

## Find the owner

- [`engine.rs`](src/engine.rs): the public operations and policy.
- [`db.rs`](src/db.rs): private SQLite transactions and guarded state changes; the only lifecycle write authority.
- [`resources.rs`](src/resources.rs): accepted resource snapshots.
- [`attachments.rs`](src/attachments.rs): the capture store: confined streaming copies, publication, verification and reconciliation.
- [`intake.rs`](src/intake.rs): accepting events with attachments: bounded, coalesced captures with storage reservations.
- [`runtime_client.rs`](src/runtime_client.rs): concrete HTTP execution transport. A turn's request has no total deadline: it takes as long as the runtime's work, and a caller that stops waiting ends it, which the runtime takes as cancellation.
- [`application/http.rs`](src/application/http.rs): authentication, request/response formatting, runtime workers and service assembly.
- [`application/admin.rs`](src/application/admin.rs), [`application/cli.rs`](src/application/cli.rs): bundled installation defaults, operator commands and startup.
- [`application/operations.rs`](src/application/operations.rs), [`application/update.rs`](src/application/update.rs): the installed release's user services, logs, health and archive updates. The maintenance lock they take is `carillon_installation::maintenance`, shared with the Discord provider's command module changes.

The CLI and the direct example use the same `Core`. Application assembly stays ordinary modules in this crate: a second core service or a generic provider framework would add no useful boundary.
