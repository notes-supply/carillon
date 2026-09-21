# Durable core

[`Core`](src/engine.rs) owns admission, conversation/runtime ownership, execution outcomes and delivery operations over real SQLite. It has no HTTP request types, credentials, bundled provider choices, or process startup. [`examples/conversation.rs`](examples/conversation.rs) is the shortest complete caller:

```sh
mise run example:core
```

The example creates temporary storage, accepts and deduplicates a message, invokes an explicitly deterministic in-process runtime, reads the captured resource snapshot, and observes/acknowledges delivery. No Discord or model account is used. The example is also executed by `cargo test`.

## Construct and use

- `Core::create(CoreOptions)` creates fresh storage; `Core::open(CoreOptions)` requires an existing compatible database. Options name the agent, resource root, default runtime and concrete runtime descriptors. The application supplies these facts; core never chooses Pi or Discord.
- `accept(interface, event)` returns a durable receipt. **The caller must authenticate external ingress before supplying the interface identity.** Native in-process callers have the same Unix authority as any other code in that process; this API is not a second permission system.
- `runtime_for(interface_id, query)` resolves the conversation owner for model operations; existing conversations do not follow a changed default.
- `execute_next(runtime_id, operation)` claims one eligible turn, calls the supplied runtime operation, validates its answer and commits completion plus delivery intent atomically. It is one step, not an implicit background worker. Verify local transport prerequisites before claiming. [`RuntimeClient`](src/runtime_client.rs) is the service's concrete authenticated HTTP collaborator; an in-process caller can supply an ordinary async function instead.
- `claim_delivery`, `acknowledge_delivery` and `reconcile_deliveries` preserve interface-instance ownership and delivery uncertainty. They never infer that ambiguous delivery is safe to repeat.
- `recover_interrupted` is explicit. Call it only when taking exclusive ownership after the old worker has stopped. Opening another handle or running diagnostics is not a restart. `Core` itself starts no tasks and requires no shutdown routine; the application owns workers, cancellation and the process lock.

`TurnFailure::Retry` means the runtime **provably did not receive** the work. Anything ambiguous is `Uncertain`, terminally interrupting that turn and leaving the conversation usable for later input. A failed database outcome write remains running until explicit startup recovery. There is no alternate lifecycle store, generic repository, or transcript reconstruction.

## Find the owner

- [`engine.rs`](src/engine.rs): the public operations and policy.
- [`db.rs`](src/db.rs): private SQLite transactions and guarded state changes; the only lifecycle write authority.
- [`resources.rs`](src/resources.rs): accepted resource snapshots.
- [`runtime_client.rs`](src/runtime_client.rs): concrete HTTP execution transport.
- [`application/http.rs`](src/application/http.rs): authentication, request/response formatting, runtime workers and service assembly.
- [`application/admin.rs`](src/application/admin.rs), [`application/cli.rs`](src/application/cli.rs): bundled installation defaults, operator commands and startup.

The CLI and the direct example use the same `Core`. Application assembly stays ordinary modules in this crate: a second core service or a generic provider framework would add no useful boundary.
