# Carillon architecture

## System shape

```text
external platform <-> interface provider <-> Carillon core <-> runtime provider <-> agent engine
```

The included providers are Discord and Pi. They are examples of the two provider roles, not fixed architectural dependencies.

## Core

The Rust core owns:

- configured agent, interface, and runtime identities;
- accepted external events and duplicate suppression;
- conversation continuity and runtime selection;
- the queued, running, completed, and uncertain turn states;
- opaque runtime continuation references;
- delivery intents and their outcomes; and
- inspection, backup, and restore operations.

SQLite is the only writer-owned durable store for this lifecycle. Network and model work never run inside its transactions.

## Interface providers

An interface provider owns its platform credential and protocol. It authenticates ingress, maps platform conversations to Carillon audience IDs, submits admitted events, claims delivery work, and performs native delivery.

The Discord provider additionally owns guild/user/route admission, mention handling, Discord thread creation and adoption, typing, reactions, and message splitting. Core does not model Discord hierarchy.

A future interface provider can map another platform to the same accepted-event and delivery boundaries without teaching core that platform's API.

## Runtime providers

A runtime provider receives a claimed turn containing agent, interface, audience, input, resource, and optional continuation identity. It returns output and an opaque continuation reference.

The provider owns everything native to its engine. For Pi this includes sessions, model execution, tools, extensions, skills, compaction, settings, and usage. Core does not offer a generic tool or extension protocol on top of Pi.

A future runtime can use different native state and features. Runtime replacement does not imply transcript or extension portability.

## Operation lifecycle

1. An interface provider authenticates and admits an external event.
2. Core transactionally records the event and a queued turn, then returns a queued receipt.
3. Core runs a worker for each enabled runtime, including non-default owners of accepted work. Each claims its next eligible turn and marks it running.
4. The runtime attempts execution. Carillon does not guarantee exactly-once model or tool side effects.
5. On success, core atomically stores completed output, the continuation reference, and a delivery intent.
6. The interface provider claims and sends that delivery, then records delivered or uncertain status.

If the runtime call or completion commit has an ambiguous outcome, the turn becomes uncertain. On startup, any turn still marked running becomes uncertain. Carillon does not infer that it is safe to repeat model work.

An uncertain turn blocks later turns for the same interface, agent, and audience, but does not block unrelated conversations. An operator can explicitly retry or skip it.

## Configuration and supervision

Provider instances are declared in configuration with stable IDs and one of two roles: interface or runtime. An agent names its runtime provider. Provider state paths are namespaced by provider identity.

The included release supervises core, Discord, and Pi as separate user services. They share the installing Unix identity but can restart independently. Provider configuration is loaded at process start; changes are applied by restarting the affected service.

## Trust and authority

Remote admission and local execution are separate boundaries. Interface providers fail closed before runtime work. After admission, the services have the ordinary authority of their Unix owner, including that owner's groups, files, executables, applications, and configured passwordless sudo.

Carillon does not create another permission model. Engine-native extensions may add approval, confinement, or specialized credentials where needed.

## Beta posture

The architecture protects current behavior rather than old private-beta formats. There is no migration framework or compatibility parser. Changes to state or configuration use a fresh installation or an explicit one-off cutover when actually required.
