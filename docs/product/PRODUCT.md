# Carillon product brief

## Product

Carillon connects external messaging systems to agent runtimes on one Linux host.

Discord and Pi are the first providers. They prove the provider boundaries but do not define every future interface or runtime. The core keeps only the shared work: admission, agent and conversation identity, operation state, runtime selection, and delivery intent.

This structure lets the system gain new interfaces, runtimes, and runtime-native tools without putting all of their behavior in one service. It should also remain understandable enough for agents to modify safely.

## First useful release

An operator can:

1. install Carillon as an ordinary Unix user;
2. configure a private Discord guild, allowlisted users, and explicit channel routes;
3. converse through Pi with channel- or thread-scoped continuity;
4. use Pi-native tools, extensions, and skills with the owner's Unix authority;
5. restart the Discord or Pi provider independently;
6. inspect recent turns and current provider/resource identity; and
7. back up and restore the stopped installation.

## Provider model

Carillon has two provider roles:

- **interface providers** own external-platform connections, admission, routing, and delivery;
- **runtime providers** translate Carillon turns into a native agent engine and own its continuations and resources.

Provider instances have stable IDs. An agent selects the default runtime for new conversations. The first accepted turn pins its conversation to that runtime, including while queued and before a native continuation exists. Changing the default does not move accepted work or existing conversations. Disabled or removed owners are not silently replaced; re-enable or restore that provider to continue its conversations.

Provider configuration is static during a process lifetime. Adding or replacing a provider is an explicit configuration and supervision change, followed by restarting the affected service. Carillon does not attempt hot plugin loading or automatic translation between unlike runtimes.

## Requirements

### Admission

Discord rejects direct messages and any event outside its configured guild, immutable user allowlist, and immutable channel routes before submitting work to core. Routes choose mention or always activation and thread- or channel-scoped continuity. Repeated submission of the same external event does not start another turn.

### Operation lifecycle

Once core accepts an event, it persists a queued turn. A core worker claims queued work and submits it to the selected runtime provider. Ambiguous execution is not automatically replayed; this is not an exactly-once guarantee for model or tool side effects. Completion stores the output, continuation reference, and delivery intent atomically.

A running turn whose outcome can no longer be proved becomes uncertain. It is not automatically replayed, and later work for the same conversation waits for operator action. Delivery has a separate durable outbox so a lost interface connection does not require rerunning the model.

### Runtime ownership

The runtime provider owns native sessions, model execution, tools, extensions, skills, and engine-specific settings. Core stores an opaque continuation reference and does not reinterpret native history or plugin state.

### Unix authority

All included services run as the installing Unix user. Carillon adds no command broker, path allowlist, approval database, privilege proxy, or general permission system. Optional confinement or approval belongs in the runtime or its extensions.

### Beta policy

Carillon is currently a private beta. State and configuration use the current schema only. When those formats change, a fresh installation or explicit cutover is preferred over speculative compatibility and migration code.
