# Carillon product brief

## Product

Carillon connects external messaging systems to agent runtimes on one Linux host.

Discord and Pi are the first providers. They prove the provider boundaries but do not define every future interface or runtime. The core keeps only the shared work: admission, agent and conversation identity, operation state, runtime selection, and delivery intent. Each provider is its own program, launched through the one `carillon` command. Bundled providers are installed beside core; independently installed providers are found on the owner's `PATH`.

This structure lets the system gain new interfaces, runtimes, and runtime-native tools without putting all of their behavior in one service. It should also remain understandable enough for agents to modify safely.

## Current release behavior

The following describes the existing release, not a fixed implementation contract. Its component structure and lifecycle policies can change with the product.

An operator can:

1. install Carillon as an ordinary Unix user;
2. configure a private Discord guild, allowlisted users, and explicit channel routes;
3. converse through Pi with channel- or thread-scoped continuity, including files and images attached to messages;
4. use Pi-native tools, extensions, and skills with the owner's Unix authority, and follow what a running turn is doing in a temporary Discord message;
5. choose the model and its engine settings for the agent or for one conversation with `/model` in Discord, and run the runtime's own commands and skills in a conversation with `/command`;
6. restart the Discord or Pi provider independently, and install or reset Discord's command module without restarting it, keeping an installed module across restarts and updates;
7. inspect recent turns and current provider/resource identity; and
8. back up and restore the stopped installation.

## Provider model

Carillon has two provider roles:

- **interface providers** own external-platform connections, credentials, admission, routing, platform commands, and delivery;
- **runtime providers** translate Carillon turns into a native agent engine and own its continuations, resources, models, and engine settings.

Provider instances have stable IDs. An agent selects the default runtime for new conversations. The first accepted turn pins its conversation to that runtime, including while queued and before a native continuation exists. Changing the default does not move accepted work or existing conversations. Disabled or removed owners are not silently replaced. New input for those conversations is rejected with a configuration conflict (and Discord failure feedback), not retried indefinitely; already accepted queued work remains pinned and waits. Re-enable or restore the owner and resubmit rejected input to continue.

Providers are independently installed programs. Core is platform-neutral and is not rebuilt to add or replace a provider; a provider's own settings live in its own file, and `carillon <provider>` launches it. The current implementation loads registry and provider configuration at startup and applies changes by restarting the affected service. Discord's commands and interactive controls are the exception: they live in a module the running provider can replace without reconnecting to Discord. Runtime replacement does not imply automatic translation of native state between unlike engines.

## Requirements

### Admission

Discord rejects direct messages and any event outside its configured guild, immutable user allowlist, and immutable channel routes before submitting work to core. Routes choose mention or always activation and thread- or channel-scoped continuity. Repeated submission of the same external event does not start another turn.

### Attachments

Files uploaded with an admitted Discord message are downloaded only after admission and kept by core with the turn; the message is accepted with all its files or refused with a short explanation. Every file is available to the agent's ordinary tools, and suitable images are also given to the model as images. Nothing is extracted, converted or executed on arrival. Interface, core and runtime each bound counts, sizes, concurrency and storage, and accepted files are never deleted to make room.

### Operation lifecycle

Once core accepts an event, it persists a queued turn. A core worker claims queued work and submits it to the selected runtime provider. Ambiguous execution is not automatically replayed; this is not an exactly-once guarantee for model or tool side effects. Completion stores the output and delivery intent atomically.

A running turn whose outcome can no longer be proved, or that the runtime reports as failed, becomes interrupted. It is not automatically replayed. The conversation is told, with the runtime's explanation when it has one that is safe to show, such as a provider's usage limit. The next user message uses the same saved native session without an operator decision; already queued user messages proceed in order once the runtime is idle. Delivery has a separate durable outbox so a lost interface connection does not require rerunning the model.

### Activity visibility

While a turn is queued or running, its Discord conversation shows a temporary card after a short delay: the current operation with its relevant target (a Bash command's first line, a file path, a search pattern, a Discord action and target), a few recent calls, and a phase label when no call runs. Rapid work updates the card at most every three seconds and only when it changes. When the runtime runs child agents, hands the conversation's context to another model or forks it, the card lists that work too, with the state the runtime last confirmed and an honest note when it cannot see all of it. Work an answer started keeps its card after the answer is delivered, until the runtime has reported it. It never shows tool output, prompts, full scripts or private reasoning; redaction of what it does show is best-effort. The card is removed once a completed turn's answer is delivered and otherwise says honestly how the turn ended. Delivery of the answer never depends on the card.

### Runtime ownership

The runtime provider owns native sessions, model execution, tools, extensions, skills, and engine-specific settings. The runtime selects and reopens its native session from the conversation identity. Core does not store a parallel session reference or reinterpret native history or plugin state.

### Model selection

`/model` shows the effective provider, model, and settings for the conversation it is used in and for the agent default, and whether the conversation follows the default or overrides it. Either scope can be changed or returned to its default through Discord-native controls. The agent default is that agent's, not every agent sharing a runtime. The panel opens on the selected scope's active provider and model. Browsing another provider only filters the model list; it does not change either scope. Choosing a model applies it. Every model the runtime offers remains reachable: each provider's models are paged 25 at a time, with the visible range and total shown for overflow. Provider-menu overflow has navigation in the menu itself. Settings are shown for the active model when browsing its provider, at the values that will run: a setting no scope has chosen shows the model's default. Every change is confirmed by the runtime before it is shown as made; a refused or unconfirmed change is shown with the runtime's reason above the current settings and their controls, without being retried, and a control left over from an earlier catalogue is reported rather than misapplied.

Selection stays within the conversation's runtime; it is not runtime migration. Queued work runs with the selection in force when it starts; running work keeps the selection it started with.

A scope overrides the whole selection, not individual fields. Choosing a model starts from that model's defaults. Changing a setting saves that model with its currently resolved option values for the scope; old controls are refused if their model changed, and a concurrent selection change during the operation is rejected rather than overwritten; returning to the agent or runtime default clears the whole override. There is no per-option inheritance editor.

The current Pi provider exposes thinking levels where the model supports them, not a Standard/Fast speed control. Model browsing is paginated without a catalogue-size cap. Settings menus currently show at most 25 settings and 25 choices per setting; Pi's thinking control fits within that bound. A future runtime offering larger settings lists needs additional navigation before all its options are usable. Discord modals and autocomplete are not implemented.

### Unix authority

All included services run as the installing Unix user. Carillon adds no command broker, path allowlist, approval database, privilege proxy, or general permission system. Optional confinement or approval belongs in the runtime or its extensions.

### Beta policy

Carillon is currently a private beta. State and configuration use the current schema only. When those formats change, a fresh installation or explicit cutover is preferred over speculative compatibility and migration code.
