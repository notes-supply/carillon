# Carillon operations

## Supported environment

The first release is a self-contained composition service for one supported Linux host. It includes the Carillon core, initial interface and runtime adapters, example extensions and capabilities, service definitions, migrations, and the files needed to create an agent. Running it does not require a source checkout or development toolchain.

Core, interface adapters, runtime adapters, interface extensions, and capabilities run as separately supervised services. Each receives the credentials, files, network access, and durable paths appropriate to its role.

## Setup and everyday operation

Setup establishes service identities, durable state, credentials, initial components, and an agent resource root, then reports what the operator needs to pair Discord and begin a conversation.

The operator can see the health and selected identity of the core, adapters, extensions, capabilities, agents, native continuations, queued work, storage, and latest backup. Turn inspection connects the external event, participating components and resources, runtime-native evidence, capability activity, and delivery result without unnecessarily exposing prompts or credentials.

## Live extension and restart

An operator can make a new or changed interface adapter, runtime adapter, interface extension, or capability available; select it for an agent or route; and restart or replace the affected component without stopping unrelated parts. Agent-file and runtime-native extension changes likewise become available to later turns without rebuilding Carillon.

Carillon reports whether active work continued, completed, or was interrupted rather than silently moving it across incompatible code or resources. Component lifecycle is observable and deliberate, but the implementation is free to choose the simplest activation and supervision mechanics that provide that behavior.

Interface extensions can add commands or interactions without receiving the platform token. Runtime adapters expose their actual supported features and native resource/session identities. Switching runtime adapters does not silently translate incompatible native state.

## Credentials

Credentials are scoped to their consumers: platform credentials to interface adapters, provider credentials to runtime adapters, and extension or capability credentials to the relevant component. They remain outside normal state, session history, diagnostics, and logs. Storage should use the host's encrypted service-credential mechanism where available.

## Backup and restore

A backup captures a consistent view of Carillon's database and configuration, common agent files, runtime-owned resources and sessions, selected component identities and configuration, component-owned durable state, and the release information needed to interpret them. Portable encrypted credentials can be included when the host credential mechanism permits it.

Restore reconstructs a coherent installation and reconciles core and component state before accepting new work. The operator can continue from the next safe action without repeating completed runtime work or an external delivery whose outcome is uncertain.

## Upgrades

Components can evolve independently when their public Carillon boundary remains compatible; coordinated upgrades remain possible when a boundary itself changes. Native runtime formats remain owned by their runtime adapters and use their supported compatibility behavior rather than being rewritten into one Carillon session format.

The exact commands, filesystem layout, packaging, transport, diagnostics, and release workflow belong to the implementation. The product requirement is an understandable installation that can be extended, restarted in parts, operated, and recovered without repository knowledge.
