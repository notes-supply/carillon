# Durable core

[`Core`](src/engine.rs) owns admission, conversation ownership, turn outcomes
and delivery over SQLite. It has no HTTP types, credentials or process
startup. [`examples/conversation.rs`](examples/conversation.rs) runs a whole
conversation against temporary storage and a deterministic in-process
runtime, with no Discord or model account; `cargo test` runs it too.

```sh
mise run example:core
```

## Using it

- `Core::create(CoreOptions)` creates fresh private storage and refuses to
  adopt existing files; `Core::open` opens storage of the current format only.
- `accept(interface, event)` returns a durable receipt. Authenticate external
  ingress before passing an interface identity. An executable event with files
  is accepted only once core has its own verified copy.
- `event_status(interface, event_id)` says whether an event was accepted, is
  still being captured, or is unknown, for an interface settling a lost answer.
- `run_next(runtime, lane, stopping, operation)` claims one turn of a lane
  (`Lane::Messages` or `Lane::Observations`), calls the supplied runtime
  operation and commits the outcome with its delivery intent. Once `stopping`
  is cancelled it claims nothing more; a claimed turn still settles. `work(lane)`
  changes when a turn may have become claimable, so workers wait instead of
  polling.
- `claim_delivery`, `acknowledge_delivery` and `reconcile_deliveries` keep each
  delivery with the interface instance that claimed it, and never treat an
  uncertain send as safe to repeat.
- `jobs_at_work` and `named_jobs` read an interface's jobs: a message's turn
  and the observations that reported its delegated work.
- `recover_interrupted` and `reconcile_attachments` are for exclusive
  startup, before events are accepted. Recovery interrupts queued turns whose
  agent moved or was removed, including observations, even if their former
  runtime is disabled or no longer configured. Running messages keep their
  restart notice; running observations return to the queue before this check.

Storage calls are synchronous. Async paths run SQLite writes and resource
snapshots on Tokio's blocking pool.

Conversations remain pinned to their original runtime. If the agent moves to
another runtime or is removed, core accepts further messages as interrupted
turns and commits their explanation and delivery intent with the receipt.
This needs no enabled or configured former runtime and captures no new agent
resources or attachments. An unknown agent cannot start a conversation or
begin attachment capture. Duplicate events keep their original turn. Model
and command queries instead return the explanation as a conflict. New
conversations use the agent's configured runtime. Agent assignments change
only on restart; `set_runtimes` changes runtime availability live.

A runtime operation's failure decides what happens next.
`TurnFailure::Retry` means the runtime provably never received the turn, so
it is queued again. `Unanswered` asks an observation again and interrupts a
message. `Uncertain` and `Reported` interrupt the turn, with the runtime's
explanation when it gave one. An interrupted turn is never replayed.

## Where things live

- [`engine.rs`](src/engine.rs): public operations and policy.
- [`db.rs`](src/db.rs): every lifecycle write, in SQLite transactions.
- [`resources.rs`](src/resources.rs), [`attachments.rs`](src/attachments.rs),
  [`intake.rs`](src/intake.rs): captured resources and files.
- [`runtime_client.rs`](src/runtime_client.rs): the HTTP runtime transport.
- [`application/`](src/application): the `carillon` command and service. In
  `http/`, `registry.rs` serves the registry and its live apply,
  `workers.rs` supervises the runtime workers, and `maintenance.rs` serves the
  maintenance pause. `drain.rs`, `operations.rs` and `update.rs` run restarts,
  services and updates.
