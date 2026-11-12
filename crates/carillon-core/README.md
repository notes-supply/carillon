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
- `dispatch_next(runtime, stopping, submit)` hands a runtime its oldest
  queued message, with no wait for the conversation's earlier turns. The
  turn becomes running once the runtime takes it; `InboxFailure::Later`
  and `Misinstalled` leave it queued to be handed over again, in order, and
  `Refused` interrupts it with the runtime's explanation. Once `stopping` is
  cancelled it hands over nothing more. `work()` changes when a turn may
  have been queued, so a dispatcher waits instead of polling; the service's
  dispatcher backs off from 1 to 60 seconds while its runtime takes
  nothing, logging each new kind of failure and the recovery once.
- `settle(runtime, turn, outcome)` records how a turn the runtime holds
  ended. An answer's files are copied first, unless the turn already ended.
- `claim_delivery`, `acknowledge_delivery` and `reconcile_deliveries` keep each
  delivery with the interface instance that claimed it, and never treat an
  uncertain send as safe to repeat. For an interface in
  `CoreOptions::retained`, each delivery is written as delivered with its
  turn's end, by the one function in [`db.rs`](src/db.rs) that writes a
  delivery, so none written while it retains waits for a claim.
- `jobs_at_work` and `named_jobs` read an interface's jobs, one turn each.
- `conversations` and `transcript` read an interface's own history: its
  conversations, the latest active first, and one conversation's turns, each
  with what began it and the answer core recorded. Both page backwards from
  the newest with `before`. The list reads conversation rows and the
  unfinished-turn index, no turn row and no message text. A conversation's
  activity marker rises with every turn accepted, opened or ended. An
  interface polls the list, compares each conversation's `activity` and
  `at_work`, and reads a transcript again only when one of them changed. A
  turn's state while it is unfinished, and an external answer's delivery
  state, are not covered: an interface that shows them reads the jobs of
  its unfinished turns.
- `recover_interrupted` and `reconcile_attachments` are for exclusive
  startup, before events are accepted. Recovery returns the turns core
  handed a runtime to the queue, interrupts those of a runtime no longer
  configured, and interrupts queued turns whose agent moved or was removed,
  even if their former runtime is disabled.

Storage calls are synchronous. Async paths run SQLite writes and resource
snapshots on Tokio's blocking pool.

The latest turn owns a conversation's runtime assignment. If the agent moves
runtimes, core accepts the first later message as interrupted and records it
under the current runtime. The next message in the same audience starts a fresh
native conversation there. If the agent is removed, messages remain stranded.
The interruption, explanation and delivery intent commit with the receipt.
This needs no enabled or configured former runtime and captures no new agent
resources or attachments. An unknown agent cannot start a conversation or
begin attachment capture. Duplicate events keep their original turn. Model
and command queries instead return the explanation as a conflict without
transferring ownership. Startup settlement preserves the former runtime too. New
conversations use the agent's configured runtime. Agent assignments change
only on restart; `set_runtimes` changes runtime availability live.

A runtime holds the turns it takes and settles each itself, with its own
credential: answered, failed, steered into the running turn or stopped. The
outcome and its delivery commit together, once; the same outcome again is
recorded, another is refused. Steered and stopped turns deliver nothing to
send, only their silence. A runtime may also open a running turn in one of
its own conversations, idempotent by its key, and read back the turns core
records running on it, with each opened turn's key, to take them up again
after a restart. Core relays an interface's control of a running turn to it
without keeping anything of the control. At a restart, the turns core
handed a runtime go back to the queue to be handed over again, and the
turns it opened stay running. Nothing in core waits on a running turn, and
another runtime's turns never hold back a conversation's messages.

## Where things live

- [`engine.rs`](src/engine.rs): public operations and policy;
  [`engine/dispatch.rs`](src/engine/dispatch.rs) and
  [`engine/settle.rs`](src/engine/settle.rs) hand turns to runtimes and
  record what they say of them.
- [`db.rs`](src/db.rs): every lifecycle write, in SQLite transactions, with
  its submodules for admission, job reads, history reads, recovery and held
  turns.
- [`resources.rs`](src/resources.rs), [`attachments.rs`](src/attachments.rs),
  [`intake.rs`](src/intake.rs), [`answers.rs`](src/answers.rs): captured
  resources, messages' files and answers' files.
- [`runtime_client.rs`](src/runtime_client.rs): core's calls to a runtime's
  inbox and restart readiness.
- [`application/`](src/application): the `carillon` command and service. In
  `http/`, `registry.rs` serves the registry and its live apply,
  `workers.rs` supervises each runtime's dispatcher, `maintenance.rs` serves
  the maintenance pause, `runtime.rs` serves runtimes' calls and runs their
  dispatcher, `controls.rs` relays turn controls, and `conversations.rs`
  serves an interface its history. `drain.rs`,
  `operations.rs` and `update.rs` run restarts, services and updates.
