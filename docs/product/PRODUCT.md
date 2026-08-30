# Carillon product brief

## Product

Carillon lets an individual operator run durable private agents on one Linux host. New interfaces, agent engines, interface features, and capabilities can be added or restarted independently without turning every extension into a core rebuild or whole-service outage.

Carillon should feel like a service rather than a development environment: install it, create an agent from readable files, connect an identity, converse, extend what the system can do, inspect what happened, and recover it.

## User journey

For the first useful release, an operator can:

1. install Carillon without a development toolchain;
2. create an agent from a plainly named system-prompt file and runtime resources;
3. configure one private Discord server by immutable guild ID, allowlist their immutable Discord user ID, and hold a continuing conversation through Pi only inside that server;
4. add an externally implemented guild-scoped Discord command or interface feature without rebuilding the Discord adapter;
5. enable a Pi-native extension and an external agent capability;
6. use those additions on subsequent turns while unrelated parts remain available;
7. restart the Discord adapter, Pi runtime adapter, an interface extension, or a capability independently and continue safely;
8. inspect which resources and component versions shaped a turn; and
9. back up the installation, restore it, and continue the conversation.

That continuous journey is the definition of the first product. The implementation may reach it in whatever sequence and internal shape proves simplest.

## Composition and extensibility

Carillon has four logical extension roles:

- **interface adapters** own connections to external platforms such as Discord;
- **runtime adapters** translate Carillon's agent lifecycle into a native engine such as Pi or, later, Codex;
- **interface extensions** contribute commands, interactions, presentation, or other interface features through an adapter's public extension surface; and
- **capabilities** provide independently implemented operations agents can use.

These are separately supervised parts connected through public Carillon boundaries. They are not required to share a language or release cycle, and the core does not hardcode Discord or Pi as the only possible implementations.

An operator can add, configure, activate, deactivate, replace, or restart one part without stopping unrelated parts. New work uses the selected configuration; work already in progress remains coherent or becomes honestly interrupted. Carillon retains enough component and resource identity to explain and recover a turn.

### Interface extension

An interface adapter exposes the platform primitives external features need without giving those features the platform credential. An external interface extension can contribute commands or interactions and receive authorized routed invocations. The credential-owning adapter performs native registration, deadlines, rate limits, and platform side effects.

Platform-neutral identity, routing, causality, and delivery remain visible to the core. Platform-native payloads remain available through the owning adapter rather than being flattened into text or promoted into universal Carillon concepts.

### Runtime independence

Carillon core talks to a runtime-adapter contract, not directly to Pi. That contract covers the small lifecycle Carillon actually owns: selecting or opening a native continuation, submitting input, observing output and lifecycle events, cancellation or steering when supported, and reconciling durable native evidence after interruption.

Runtime adapters report their supported features and retain opaque native references and lossless native metadata. Carillon does not pretend every engine has Pi's tools, branches, compaction, extension hooks, or session format. Unsupported features remain explicit rather than being emulated badly through a lowest-common-denominator model.

A Pi runtime adapter owns Pi sessions, resources, tools, skills, extensions, compaction, usage, and terminal evidence. A later Codex runtime adapter can implement the same Carillon lifecycle using Codex-native concepts and storage. Runtime independence does not promise automatic transcript, extension, or configuration portability between unlike engines.

### Native extensions and capabilities

Pi-native extensions remain native Pi extensions. They can alter prompts, register tools, contribute resources, and preserve Pi state through Pi's APIs. Carillon does not force them through another universal plugin abstraction.

External capabilities use the system-level boundary when they need independent lifecycle, language, credentials, trust, or recovery. A runtime adapter presents selected Carillon capabilities to its engine through the engine's natural tool or MCP mechanism.

## Product requirements

### Identity and audience

Discord is fail-closed to one configured private guild and explicitly allowlisted immutable user identities. Direct messages, events from other guilds, and events from users outside the allowlist—even inside the private guild—are rejected before model or capability work.

Authorization and routing use immutable platform identities. Human-readable names remain presentation metadata. An accepted event stays associated with its source audience, selected agent, interface adapter, runtime adapter, native continuation, and selected extensions and capabilities.

Repeated delivery of the same external event does not create repeated model work or repeated replies.

### Agent configuration

An agent has readable common files and a resource root interpreted by its selected runtime adapter. The operator can edit the system prompt and contextual instructions as files. Runtime-native settings, extensions, skills, and sessions remain owned by that runtime adapter.

Carillon provides composition, lifecycle, explicit resource selection, and inspection. It does not need a prompt database or another prompt-composition language to expose facilities the runtime already has.

### Durability and honest recovery

Once Carillon accepts an external event, it preserves enough state to determine what may safely happen next after a component or host restart.

Completed runtime work is not rerun merely because a notification was lost. An external side effect that may have succeeded is not reported as definitely failed or automatically repeated. Recovery prefers an honest operator-visible state over fabricated certainty.

### Trust boundaries

Each component receives the credentials and files needed for its role. Platform credentials belong to interface adapters; provider credentials belong to runtime adapters; extension or capability credentials belong to their consumer. They do not belong in prompts, conversation history, ordinary application state, or logs.

Runtime-native extensions are trusted within their runtime's operating-system permissions. Separately supervised components provide the stronger boundary when code needs separate trust, language, credentials, or lifecycle.

### Operation

Carillon runs as a small one-host service with durable local state and no external database or queue. The operator can understand component health, inspect a turn without exposing secrets, change or restart a part deliberately, and make a restorable backup covering Carillon-owned and component-owned durable state.

## First-release experience

The first release demonstrates the composition model with a Discord interface adapter, one external guild-scoped Discord command extension, a Pi runtime adapter, one Pi-native extension, one Agent Skill, and one harmless external capability. It supports live activation, independent component restart, private-server conversation continuity, and stopped-service backup and restore.

It is successful when someone outside the design process can install it and complete the whole user journey without understanding the repository or manually repairing state.
