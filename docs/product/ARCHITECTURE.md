# Carillon architecture

This document describes the implemented system. Its process layout, packaging, and protocols are design choices, not requirements that future changes must preserve.

## System shape

```text
external platform <-> interface provider <-> Carillon core <-> runtime provider <-> agent engine
```

The included providers are Discord and Pi. They are examples of the two provider roles, not fixed architectural dependencies.

## Core

The Rust core (`carillon-core`, the `carillon` executable) owns:

- configured agent, interface, and runtime identities;
- accepted external events and duplicate suppression, including immutable copies of the files they carry;
- conversation continuity and runtime selection;
- the queued, running, completed, and interrupted turn states;
- delivery intents and their outcomes;
- relaying model-selection calls from an interface to the runtime that owns the conversation; and
- inspection, backup, and restore operations.

SQLite is the only writer-owned durable store for this lifecycle. Network and model work never run inside its transactions.

The durable core is platform-neutral. `Core` exposes admission, pinned runtime lookup, one-step execution and delivery operations over one private SQLite authority. Its constructor receives agent/resources/runtime facts; it does not know installation credentials, parse HTTP, or choose Discord/Pi. The application modules resolve installation defaults, authenticate ingress, assemble workers and translate HTTP into these same operations. See [the core component guide](../../crates/carillon-core/README.md) and its executable temporary-SQLite example.

## Providers are separate programs

Each provider is its own executable, built and packaged separately from core. `carillon-protocol` owns their JSON wire types; `carillon-installation` owns installation registry (`carillon.toml`), credential and filesystem utilities. The wire crate has no installation dependency and is also usable by the Wasm command module. Shared model semantics and Rust/TypeScript conformance fixtures are documented in [`packages/protocol`](../../packages/protocol/README.md). Adding an interface for another platform means adding another executable that speaks this protocol, not modifying or rebuilding core.

`carillon <provider> …` is the single user-facing entry point. Core forwards to the executable `carillon-<provider>` by replacing its process, so signals and the exit status are the provider's own. It passes the installation it resolved as `CARILLON_HOME`; `carillon-installation` owns that one rule: `--home`, then `CARILLON_HOME`, then the installation containing the running program, otherwise `~/.carillon`. Core subcommands are reserved; other names using the provider-ID character set are forwarded: `carillon discord serve …` runs `carillon-discord serve …`. The bundle's own providers sit beside the `carillon` binary and take precedence; a provider installed independently is found as an executable on `PATH`. The release owns `app/` wholesale and replaces it on upgrade, so an independent provider is installed elsewhere and survives upgrades.

Executable names and registry instance IDs are separate: `carillon discord` can serve an instance named `discord-private`. A provider executable must not use a core subcommand name, but that restriction does not apply to instance IDs.

A provider's own settings live in `providers/<id>.toml` beside `carillon.toml`. Core never reads that file; the provider and its `configure` command own the schema. The registry in `carillon.toml` records only what core needs about a provider: ID, role, expected version, loopback URL, credential hash, and whether it is enabled.

## Interface providers

An interface provider owns its platform credential and protocol. It authenticates ingress, maps platform conversations to Carillon audience IDs, submits admitted events, claims delivery work, and performs native delivery.

The Discord provider (`carillon-discord`) additionally owns guild/user/route admission, mention handling, downloading admitted messages' attachments, Discord thread creation and adoption, typing, reactions, temporary activity cards, message splitting, mention-free delivery (delivered text never notifies users, roles, `@everyone` or `@here`), application-command registration, and interaction handling. Core does not model Discord hierarchy or interactions. Its library exposes message/interaction admission with explicit configuration and participation state, and a concrete authenticated `CoreClient` independent of command hosting. Native delivery consumes those clients; the application assembles the gateway and operator server. Gateway and deterministic ingress use the same `IncomingMessage` admission path and the same attachment downloads; deterministic mode has no Discord session, so it cannot refresh an expired attachment link. See [the Discord component guide](../../crates/carillon-discord/README.md) for the gateway-free example and component-local tests.

### Activity while a turn runs

Three owners share this without a second lifecycle model:

- The runtime knows what a turn is doing. Pi's runtime derives a bounded snapshot from Pi's native session events for the lifetime of the request, from before session preparation until the turn settles: its phase, the model's latest public commentary (never thinking), the calls running now, the last few finished calls and a count of the rest. It describes each call's input beside the tool's semantics: a Bash command's first line with heredocs, later lines, substitutions, inline programs, long quoted payloads and everything after a credential-named option omitted, file paths, search patterns, and an operation's action and numeric target IDs. Unknown tools show only their name. The whole command passes the runtime's best-effort credential redaction before anything is cut, so a cut never leaves part of a recognised value; a quoted argument the redaction changed is cut entirely. It is a display boundary, not a sanitiser. `GET /v1/turns/{id}/activity?interface_id=…` answers only for a turn executing for that interface. Tool output is never included, and nothing is stored.
- Core knows which turns are queued or running. `GET /v1/activity`, authenticated like every interface call, lists only the caller's queued and running turns from SQLite and asks the owning runtimes for their snapshots at once, each with a short timeout, so the list is prompt however many runtimes are slow. A missing, slow or malformed snapshot is `null`, never a shorter list. Core keeps no cache and adds no terminal-turn query. Core trusts a configured runtime's snapshot as it trusts its answers: it checks the shape, not the size; Pi's runtime bounds what it produces, and Discord bounds what it shows.
- The Discord provider owns cards and typing. A monitor reads core every three seconds; each turn gets its own card and typing tasks watching the latest reading, so a slow or rate-limited card never delays another turn or delivery. The delivery loop reports each outcome after core acknowledges it, without waiting for the monitor. A card appears two seconds after acceptance, is edited only when its text changes and at most every three seconds, and uses Discord's relative timestamps rather than timed edits. When core stops listing a turn, its card stops showing running calls and says the turn is finishing, and each later read tries again until that edit succeeds; a failed read changes nothing. A card Discord will not edit is left alone until the outcome, then removed; one Discord has deleted is never replaced. After a completed turn's answer is delivered the card is removed; an interrupted turn or uncertain delivery leaves a short final status saying which.

Discord's [`activity/render.rs`](../../crates/carillon-discord/src/activity/render.rs) owns pure card text rendering. It receives semantic snapshots and the small concrete marker value owned and validated by the provider's [`config`](../../crates/carillon-discord/src/config.rs). The provider file's `[activity_markers]` table supplies configurable defaults, independently of reaction emoji; omitted fields use the existing appearance. Card monitoring, records, typing and bounded HTTP operations remain in [`activity.rs`](../../crates/carillon-discord/src/activity.rs). Core and Pi carry no Discord presentation settings.

Every Discord call the monitor makes is bounded, including Twilight's rate-limiter waits, its internal 429 retries and response decoding: Twilight's pre-flight check stops a queued request once its wait has passed or it is no longer wanted, and an outer timeout bounds the rest. A card is created at most once per turn: a creation Discord may have performed without confirming it, or refused, leaves the turn without a card. The provider records a card's channel and message in `activity-cards.json` in its state directory after Discord confirms it, and removes the record once the card is removed or finalized. An unconfirmed creation is recorded without a message, so a restart during the same turn does not create another; it is forgotten when the turn ends. At startup the provider deletes recorded cards before showing a replacement; a card whose removal cannot be established stays recorded and its turn gets no replacement. When the records cannot be read the provider starts without cards, leaving the file as it is, and still types and delivers. A crash between creating a card and recording it, or a failure to save the record, leaves one card no later start knows of.

Delivery never waits on the monitor, but both use the same Discord budget: Discord may place edits and messages in one learned bucket, and the global limit is always shared, so progress can delay an answer by at most what those requests consume. There is no priority scheme; progress traffic is bounded to one card creation, change-only edits at most every three seconds, one removal per turn, and one typing indicator per running turn every eight seconds.

### Attachments

Files people attach have three owners, with one path for images and other files alike:

- **Discord** decides whether anything is fetched. It admits the message by authenticated author, guild, route and activation before downloading, then streams each file from Discord's attachment CDN (HTTPS only, no redirects, no bot credential, never beyond the size Discord declared, one refresh of an expired link on the same message) into its own staging, `<data_dir>/components/<provider>/incoming/<batch>/<n>`. Generated names identify files; uploaders' names are display text. Messages are prepared in per-conversation lanes held in memory: a conversation's messages are settled one at a time in arrival order, including text behind a slow file; different conversations proceed concurrently up to a bound on active downloads and staged bytes; and a full lane or queue is refused at once, with a reply only to messages admission would activate. One preparation deadline, starting when a message's preparation begins, bounds text and files alike, from resolving the route through opening the conversation and transferring any files: a message whose route is still unknown then was never admitted and is dropped silently, and an admitted one is refused with an explanation. No submission starts after the deadline, however long feedback took, and a text submitted before it is retried until core answers; a submission core may have received is settled by asking core, so the lane is never released while that message could still be accepted. Each Discord request preparing a message is bounded as a whole, rate-limit waits and reading the answer included, and so is feedback. When the provider stops, waiting messages are dropped, preparation not yet handed to core ends (the sender is told), and submissions core may have received, like a replayed message core reports it is capturing, are settled before it exits. The provider removes a batch once core answers, and its whole staging at startup.
- **Core** owns accepted files. The event names a staged batch and lists each file's name, size and declared type, never a path or bytes; core derives the batch's directory from the authenticated interface with the installation layout helper in `carillon-installation`. Before admitting a capture, core opens every staged file beneath that directory without following links and requires a regular file of the declared length; the capture then copies the open files into a fresh directory while hashing them, so an interface clearing its staging, as one does at startup, cannot fail an admitted capture; it can only make core refuse a submission not yet admitted. It then syncs and publishes the directory without replacing anything. Only then does the acceptance transaction recheck the event's uniqueness and commit the turn with ordered `turn_attachments` references (capture, ordinal, name, size, declared type, SHA-256). Captures are core's own tasks, not the request's: a retry of the same event joins the running copy, a bounded number run at once and further submissions are refused as busy before any work, each reserves its bytes against a storage budget and free-space floor until the database decides (in full, because filesystems differ in when written bytes leave their free-space figure), and its worker stays taken until the blocking copy has actually returned, even after its deadline. A deadline or shutdown that arrives while a copy publishes still prevents a new acceptance: core rechecks before committing and removes the copy. A duplicate's losing copy is removed once the database shows it unreferenced; startup and backup reconcile abandoned and unreferenced copies under exclusive ownership. `GET /v1/events/{id}` tells an interface whether an event was accepted or is still being captured, so a lost answer is settled by asking rather than by copying again. While a capture runs, other events of its conversation are refused as unavailable and retried, so none overtakes it, whichever interface instance sends them. Core settles captures when interrupted or terminated.
- **The runtime** receives, with every turn, the installation's current attachment directory and the turn's files by relative path, and checks nothing is outside it. Core verifies each file's length and digest before dispatch and interrupts the turn instead when one changed. Pi's runtime sends a note listing the files with the message as a native custom message, delivered by Pi with the message after any compaction, or kept on its own when a command or extension consumes the message. Pi saves a new conversation only once it has a message, so a command that opens a conversation leaves nothing saved: core therefore also sends earlier files of the conversation that no answered turn has carried yet (`turn_attachments.noted`), oldest first, at most one event's worth per turn and each earlier turn whole: a file that no longer verifies is left out for good, while a turn with a file that cannot be read now waits for a later turn. The runtime notes those its Pi session lacks as files of an earlier message, through the same custom messages. An answered turn marks what it carried; the rest follow with later turns. A native extension puts the current attachment directory into a system prompt section whenever the conversation has such notes, so their relative paths resolve after reopening or relocation without rewriting Pi's history. PNG, JPEG, WebP and GIF images within the runtime's inline policy, checked from their headers with an established library, are passed as Pi's native prompt images; Pi normalizes them after extensions choose the model, and substitutes its own placeholder for text-only models. The user's text is passed unchanged.

No component inspects, converts or executes a file's contents when it is accepted, and the read-only copies are no sandbox for the owning user.

### Assistant operations

The Discord operator API also accepts `POST /v1/operations`, authenticated with the
operator credential. Concrete provider-owned operations use the existing Discord
client, validate channel/role targets against the configured guild, and rely on
Discord's own permissions and role hierarchy. They neither alter admission routes
nor create core delivery intents. Ambiguous side effects are reported as uncertain
and are not automatically replayed.

The optional [`discord-pi`](../../packages/discord-pi/README.md) package is a native
Pi extension plus skill, not a Wasm capability. A small activation tool enables
its detailed schemas in the current conversation; native Pi system messages retain
the loadout. No selection model or separate activation store exists.
`carillon discord tools install` copies the bundled extension/skill into ordinary
agent resources, with core stopped: it holds core's service lock throughout, so
no accepted turn captures one file without the other. Core captures their bytes
with accepted work, as for any resource.

Core forwards the event's opaque `native` metadata on runtime requests. Discord
constructs `native.origin` from admitted input and the resolved delivery target;
core does not interpret it. Pi's application assembly supplies the current interface
connection and opaque metadata through a per-session `carillon:context` event.
Credentials and endpoint selection stay outside prompts and captured resources.
The generic runtime/session component knows nothing about Discord. Native extension
activation is an input-context optimization, not an authorization mechanism.

### Replaceable command module

Discord's slash commands and their interactive controls live in a WebAssembly module that the provider loads at startup and can replace while running. The release ships `carillon_discord_commands.wasm`; an operator may install another. The module defines the commands, handles each interaction, and decides what to show. The provider keeps everything native: the gateway connection, admission of the interaction, every Discord and core call, authority, and deadlines. When it stops, it takes no further interaction, and waits for one it has already acknowledged to finish its invocation, native calls included, and to attempt its reply before it exits; Discord may still refuse or lose that reply.

```text
carillon CLI -> carillon-discord
                 |- native gateway, admission, Discord and core I/O
                 |- carillon-wasm host -> carillon_discord_commands.wasm
                 `- core -> carillon-runtime-pi
```

`carillon-wasm` is the shared hosting crate: bounded execution of a guest module on an established runtime (wasmtime). Every invocation runs in a fresh instance with one linear memory capped at 16 MiB and a deadline that covers the native calls it makes. This is a linear-memory limit, not a total process-memory budget. The guest reaches the native side only through one import, `carillon.call`, whose bytes the embedding provider interprets; nothing survives between invocations inside the guest, so durable state stays native. The Discord provider owns replacement: one immutable generation contains the module and its command definitions. A pointer swap activates them together; in-flight invocations retain their original generation. Changes serialize preparation, remote registration, activation and saving without blocking ordinary invocations. The crate knows nothing about Discord; another provider can embed it with its own contract, or not use Wasm at all.

The Discord contract (`carillon-discord-commands::contract`, versioned by `CONTRACT`) is JSON: the provider sends `Describe` or an admitted `Interaction`; the module answers with its command definitions, a `Reply` of content and Discord components, or a `Failure`. Through `carillon.call` the module may ask for the model state or set a model selection; the provider answers with the runtime's confirmed state or a user-facing reason. Command replies are ephemeral; a component interaction's reply updates the message carrying the component. Contract 2 adds conditional model writes; deploy the provider, command module, core, and runtime together for this protocol change. The current response contract still supports only deferred command/component replies, not modals or autocomplete. Provider browsing is presentation-only: the guest filters by provider before paginating models, carries the browsed provider in navigation controls, and writes a selection only when a model or setting is chosen (or a scope is reset).

Three facts about the module have separate owners. The provider's memory holds the generation serving now. The disk holds what the next start serves: the operator-installed module under the provider's state directory when it exists, otherwise the release's module in `app/lib`; release updates replace only the latter. The registration record holds which definitions Discord last confirmed for which application and guild. The provider is the only writer of the installed module and the record.

`carillon discord commands install FILE` sends the module to the running provider, and `reset` asks it to serve the release's module again; both are one operation in the provider, which converges the three facts in one order and runs to completion even if the operator stops waiting. It takes the installation's maintenance lock, so an update cannot check the installed state or stop the services halfway through a change. It validates the candidate, applying the same checks as `commands check` and startup: it enforces the 16 MiB module size limit, compiles it, checks the required exports, asks it to describe itself, and checks the contract version and each command definition; an unusable candidate changes nothing. When the record does not show the candidate's definitions registered, the provider registers them before the candidate serves a single interaction, first removing the record and syncing its directory, so the record never attests to definitions Discord may no longer hold. A registration Discord refuses, or one it does not answer, leaves the previous module serving. Once Discord confirms, the candidate activates even if recording the confirmation fails; that bookkeeping failure is logged and causes registration to be repeated. Only then is the change saved for later starts, by publishing or removing the installed module after syncing the state directory and its ancestors; that directory sync runs on every change, because an existing directory does not show that its entry is durable. The operator is told whether the save failed before publication (disk is unchanged; it may already contain the requested module) or after it (the new state is on disk, but a system crash may undo it). Neither report claims that the module a restart would select is usable. Repeating the operation finishes it: it registers whatever the record lacks and saves again, even when the requested module already serves. At startup the provider registers the chosen module's definitions when the record does not match, so an interrupted change costs at most a repeated registration, not a lasting mismatch.

The candidate release's `carillon discord check` validates an installed module during `carillon update`'s preflight, before services stop. A provider refuses to start with an unusable installed module, naming it. Every reply a module produces is checked against Discord's content and component limits before it is sent. Cancelling or replacing guest code never undoes a native action it already caused.

Replacement is that one explicit operation. There is one module per provider instance, no module marketplace, dependency graph, workflow interpreter, file watcher or automatic updater, and a provider speaks exactly one contract version at a time.

## Runtime providers

A runtime provider receives a claimed turn containing agent, interface, audience, input, resource identity, the attachment directory with the turn's files by relative path, and opaque interface-owned native metadata. It owns the conversation's native session and returns nonblank output. A response without usable output interrupts the turn rather than completing it silently or sending an empty reply.

A runtime's failure response may carry `{"error": "…"}`. That text explains the failure to the person in the conversation, so the runtime owns deciding what of it may be displayed. Core and interfaces show it as given and apply no text policy of their own. A runtime omits `error` when it has nothing it can safely explain, and keeps technical diagnostics in its own log. The same rule covers model calls, whose `error` Discord shows in `/model`.

The Pi runtime explains a failed turn with Pi's own failure text: the assistant's `errorMessage`, which Pi shows its own users. It covers every kind of provider failure, and failures inside Pi's agent loop, without Carillon classifying them. Pi composes that text from provider responses, so it can be a raw JSON body, an HTML page from a gateway, or appended metadata, and it can echo request details. Failures outside that text are internal and are not explained: exceptions from Carillon's runtime code, extension loading, and aborted turns. Pi's own retry settings apply as they would in Pi; Carillon does not retry.

The Pi HTTP adapter applies one conservative display boundary to every explanation it sends, including `/model` refusals that quote the request or the catalogue:

- Only the first line counts. A JSON body contributes only its conventional `message`, `error` or `error.message`.
- The whole explanation is withheld if it contains markup, a link target, an assignment (`name=value`), a labelled credential (`password: …`, `API key provided: …`), URL user information, or a `Bearer`/`Basic` value, whatever the value's length.
- URLs become `[link]`, unlabelled runs of 32 or more token characters become `[redacted]`, and at most 300 characters are shown.

The runtime log records the method and path (never the query) and a stack or message with the same structures redacted, never whole error objects. These rules are best-effort: they catch structures that carry credentials, but cannot prove that arbitrary upstream prose contains none. An unlabelled short secret in plain prose would pass.

Pi keeps a separate native session directory per interface/agent/audience and uses its native most-recent-session selection on each message. Saved history survives even an interrupted first turn; Pi may not yet have persisted a brand-new session before its first assistant message. Carillon does not reconstruct unsaved history. Runtime requests execute serially. A disconnected core request cancels native execution and waits for it to settle before releasing the queue; requests disconnected while queued never execute.

The provider owns everything native to its engine. For Pi (`carillon-runtime-pi`) this includes sessions, model execution, tools, extensions, skills, compaction, settings, usage, and the description of a running turn's activity. Core does not offer a generic tool or extension protocol on top of Pi.

The Pi package exposes `createPiRuntime` through an inert library entry point. Its instance owns execution ordering, pending operations and shutdown; callers may supply concrete Pi model/settings and selection collaborators independently. The CLI resolves installation paths and credentials, opts into provider discovery, then connects that component to the authenticated HTTP adapter. The deterministic service-test implementation is separate from native execution. See [the Pi component guide](../../packages/runtime-pi/README.md) and its direct session example.

A future runtime can use different native state and features. Runtime replacement does not imply transcript or extension portability.

### Model selection

The runtime owns model availability, engine settings, and selections. It answers `GET /v1/model` with the models it can run, each with the options it supports for that model, and the effective selection for the agent and, inside a conversation, for that conversation, marking whether each was set at that scope or inherited. `PUT /v1/model` changes the agent default or one conversation's selection, or returns a scope to its default, and answers with the confirmed state or a rejection in words meant for the user. A change may carry a structured `expected` selection (provider, model and engine-owned option values); the runtime checks it against the scope's effective selection inside the same serialized operation as the write and returns 409 without writing when it differs. Discord option controls bind to the displayed model and send this precondition, so old controls cannot retarget another model or overwrite a change made during their read/write interval.

Core relays these calls from an interface provider to the runtime that owns the conversation, adding the interface identity that names the conversation; status and body pass through unchanged. Only transport failures are described by core. Discord presents the state through `/model` and asks for changes; it keeps no settings ledger of its own.

Pi keeps Carillon's selections in `pi/model-selection.json` beside its private state. Updates flush the replacement file, rename it, and sync its directory before confirming success. The runtime default is Pi's own setting. A turn resolves its selection when it starts: conversation override, else agent default, else runtime default. Queued work therefore picks up a change; running work keeps what it started with.

## Operation lifecycle

1. An interface provider authenticates and admits an external event.
2. Core durably records the accepted event as a queued turn, capturing the current agent resources and any attached files, then returns a queued receipt.
3. Core runs a worker for each enabled runtime, including non-default owners of accepted work. Each claims its next eligible turn and marks it running. Once core begins to stop, its workers claim no further turn, and core waits for a turn already running to settle before it exits.
4. The runtime attempts execution. Carillon does not guarantee exactly-once model or tool side effects.
5. On success, core atomically stores completed output and a delivery intent.
6. The interface provider claims and sends that delivery, then records delivered or uncertain status.

If the runtime call or completion commit has an ambiguous outcome, or the runtime reports that the turn failed, the turn becomes interrupted. It is terminal: core never replays its input, and the next user message can use the same native session without an operator decision. A runtime's explanation, when it gives one, becomes part of the interruption notice delivered to the conversation; a failure report does not show that no model or tool work happened. Already queued user messages continue in order once the runtime is idle; otherwise the conversation waits for input. On startup, turns still marked running become interrupted.

Each outcome is recorded once. If storage cannot record it, the turn remains running until storage is repaired and core restarts; this is a storage failure, not a routine recovery gate. A committed answer's delivery remains independent of execution. Deliveries carry the task state so successfully sending an interruption notice cannot be mistaken for successful task completion.

## Configuration and supervision

Provider instances are declared in `carillon.toml` with stable IDs and one of two roles: interface or runtime. An agent names its runtime provider. Provider state paths are namespaced by provider identity.

The included release supervises core, Discord, and Pi as separate user services running three separate programs: `carillon core`, `carillon discord serve` (the `carillon-discord` executable, with the release's command module under `app/lib`), and `carillon-runtime-pi`.

The services share the installing Unix identity. Registry and provider configuration are loaded at process start; changes are applied by restarting the affected service. The Discord command module is the exception: it is installed or reset in the running provider by `carillon discord commands`, without reconnecting the gateway.

Operating an installation belongs to application assembly and the release scripts, not to the durable engine. The core application's `start`, `stop`, `restart` and `logs` call `systemctl --user` and `tail` directly. `update` unpacks a local release archive as regular files and checks the release's identity while the services keep running. The release's `install.sh --update --check` then verifies the payload against the packager's `MANIFEST`. It also runs the release's own read-only validation of the installed state through the existing owners: `Core::open` for the registry and core schema, and the Discord configuration loader and command module validation for its settings and any installed command module. Only after that are the services stopped. `update` then delegates to `install.sh --update`, which performs the same application exchange and service registration as first-time setup, and finally restores the previous running state. Application files, user units and systemd are not one transaction; failures report what may have changed rather than rolling back. The user units identify the installation they run, so commands for one installation do not control another's services.

## Trust and authority

Remote admission and local execution are separate boundaries. Interface providers fail closed before runtime work; Discord admits an interaction by guild, user, and channel exactly as it admits a message, before the command module runs. After admission, the services have the ordinary authority of their Unix owner, including that owner's groups, files, executables, applications, and configured passwordless sudo.

Carillon does not create another permission model. The Wasm sandbox bounds the command module's memory and time and limits what it can ask for; it is not a permission system for the owner, and native operations enforce authority and deadlines. Engine-native extensions may add approval, confinement, or specialized credentials where needed.

## Beta posture

The architecture protects current behavior rather than old private-beta formats. There is no migration framework or compatibility parser. Changes to state or configuration use a fresh installation or an explicit one-off cutover when actually required.
