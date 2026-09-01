# Carillon architecture

## System shape

```text
                         interface extensions
                                  |
Discord <-> interface-discord <-> Carillon core <-> runtime-pi <-> Pi/provider
other interfaces <-> adapters          |         <-> runtime-codex <-> Codex
                                       |
                                  capabilities
```

Carillon is a durable composition layer. The core keeps product identity and continuity coherent while interface adapters, runtime adapters, interface extensions, and capabilities remain replaceable parts with their own lifecycle.

Carillon is also the owner's general-purpose personal agent. A normal installation has one operating-system principal: the ordinary Unix user who installs it. Every component runs as that user through native user-service supervision and therefore has exactly that user's filesystem, process, executable, Git/SSH, local-application, user-session, group, and passwordless-sudo authority. Core does not add a second permission model, broker commands, constrain paths, proxy privilege, or mediate Pi extensions. Authenticated remote ingress remains a separate product boundary.

## Carillon core

The core is a Rust service and the authority for platform identities, agents, routes, selected components, admission of external events, conversation continuity, turn correlation, and delivery intent.

A local SQLite database is the durable record for those responsibilities. Components do not become additional writers to that database. Network, platform, capability, and model work happen outside database transactions.

The core stores stable Carillon identities plus opaque native references supplied by components. It knows which component and agent-resource configuration belongs to a turn, but it does not make Pi session entries, Discord interaction tokens, or another component's native state part of the universal core model.

## Component lifecycle

Components run as separately supervised processes and expose public, role-specific Carillon contracts rather than one universal event bus.

Components can be introduced, selected, replaced, or restarted while unrelated parts continue operating. The implementation decides how discovery, activation, health, transport, and compatibility work. The product boundary requires deliberate lifecycle, scoped authority, observable identity, and safe behavior for work crossing a restart.

## Interface adapters and extensions

An interface adapter owns its platform credential, authenticated connection, native protocol lifecycle, command or interaction registration, rate limits, and platform side effects. It presents durable external events and delivery results to the core while preserving native platform data without making that data universal.

External interface extensions add commands and other features through a public adapter extension surface. They receive authorized, routed events or interaction handles and return typed responses or requested actions. The adapter validates and performs native side effects, so extensions do not need the platform token or unrestricted platform access.

`interface-discord` owns the Discord admission boundary. It exposes only authenticated events from the configured private guild, explicitly allowlisted immutable users, and explicitly configured immutable channel routes; direct messages and all other traffic are rejected before crossing the adapter boundary. It also owns bot-mention parsing, parent/thread resolution, durable thread participation, typing, reactions, thread creation, and Discord delivery.

The first adapter is a separately supervised Rust Discord component using Twilight. Discord-specific hierarchy, interactions, and payloads remain Discord-native. The core receives immutable identity and causality, an opaque adapter-selected conversation audience, a separate delivery target, and delivery status without learning Discord hierarchy semantics.

## Runtime adapters

The core addresses a runtime adapter through a deliberately small lifecycle:

- create or open a runtime-native continuation for an agent;
- submit user input with stable Carillon correlation;
- observe streamed content, lifecycle events, and terminal evidence;
- request cancellation, steering, or follow-up when the runtime supports them;
- inspect capabilities and native references; and
- reconcile durable evidence after interruption.

The contract describes what Carillon needs to operate, not a universal agent message model. Runtime-specific events and metadata remain lossless and namespaced to their owner. Feature discovery lets Carillon and its interfaces expose what a runtime actually supports without fabricating parity.

### Pi runtime adapter

The first runtime adapter is a TypeScript component embedding the public `@earendil-works/pi-coding-agent` SDK. It creates headless Pi sessions from explicit immutable resources while setting Pi's `cwd` to the configured real owner workspace. Native `read`, `bash`, `edit`, and `write` tools therefore retain ordinary Pi semantics and the owner's authority, including ordinary absolute paths.

Operators authenticate through Pi's packaged login surface. Pi's global agent directory is `CARILLON_HOME/pi`, isolated from the owner's ordinary `~/.pi/agent`; the same private directory supplies `ModelRuntime` credential refresh, model discovery, inference, and Carillon service-tier defaults. An intentionally selected workspace still retains Pi-native project-local discovery and behavior.

Pi owns model execution and native session history, including messages, tools, skills, extensions, compaction, usage, and terminal evidence. Sessions remain under the Carillon agent directory; credentials, discovered catalogs, and selected models remain under Pi-private state outside immutable resource snapshots. Normal Carillon backups include this Pi-private state so restores remain usable; archives therefore contain credentials and must be protected accordingly. The adapter translates the small Carillon lifecycle to public Pi APIs and exposes runtime-neutral model discovery and selection through Carillon.

Pi session data and Pi extensions own usage and timing presentation or export. Extensions load and run through Pi's native lifecycle with the owner environment, subprocess access, and ordinary user-scoped persistence. Carillon core retains runtime evidence as opaque operational data and does not define extension permissions or statistics semantics.

### Other runtimes

A later Codex or other runtime adapter may use a different event model, resource layout, extension system, and persistence format. It supplies the common lifecycle and exposes native semantics rather than imitating Pi internally. Moving an agent between unlike runtimes is an explicit product operation if ever needed, not an accidental promise of the core abstraction.

## Capabilities

Capabilities are independently implemented operations available to agents. They may use whatever language and internal design suit the operation. A capability owns its execution and native state; Carillon owns selection, lifecycle visibility, routing, and the durable correlation needed for recovery.

Each runtime adapter presents selected capabilities through its engine's natural mechanism, such as Pi tools or MCP. Carillon does not replace a mature runtime-native tool system merely to make every engine look identical.

## Agent resources

Each agent has common human-readable instructions, a runtime-owned resource root, and an independently configured real workspace. Carillon snapshots instruction/settings/extension/skill resources for turn identity, but the runtime adapter decides how native settings, project resources, extensions, skills, sessions, tools, and prompt composition work. Carillon does not reinterpret them into a competing format or make the immutable snapshot the tool working directory.

## Conversation flow

1. An interface adapter presents an authenticated external event to the core.
2. The core durably accepts it and resolves the agent, interface extensions, runtime adapter, capabilities, and continuity.
3. The runtime adapter submits the input to its native continuation with Carillon correlation.
4. Runtime-native extensions and selected capabilities participate through the runtime's normal mechanisms.
5. The core observes native evidence and asks the interface adapter to deliver the response; interface extensions may contribute supported presentation or interaction behavior.
6. After interruption, durable core and component evidence are reconciled before work or delivery is repeated.

An external delivery that may have succeeded is exposed as uncertain rather than blindly repeated. This is product behavior, not a general-purpose effect framework.

## Implementation posture

Use mature libraries and native extension systems where they already solve the problem. Add Carillon-specific machinery for composition, identity, lifecycle, routing, durability, delivery, operation, or an observed cross-component gap.

The exact transport, schemas, supervision, package shape, queues, module layout, and implementation sequence remain engineering choices. The architecture defines replaceable boundaries while deliberately avoiding both Pi-shaped core concepts and a lowest-common-denominator agent framework.
