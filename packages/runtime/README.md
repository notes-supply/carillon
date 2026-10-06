# Shared runtime boundary

`@carillon/runtime` is what every runtime provider serves to core, independent of the engine behind it. It depends on `@carillon/protocol` and small parsing libraries only: no Pi, T3 or Discord code. Each provider package ([Pi](../runtime-pi/README.md), [T3](../runtime-t3/README.md)) depends on it and on its own engine; nothing here imports a provider.

Modules are imported by path (`@carillon/runtime/<module>`); there is no aggregate entry point.

- [`operations.ts`](src/operations.ts): `RuntimeOperations`, the interface a runtime implements: execute a turn, describe its activity, read and change the model selection, list its commands, close. `commandFinished` is the answer to a command invocation the engine completed without a reply of its own.
- [`http.ts`](src/http.ts): the authenticated HTTP adapter over those operations (`/health`, `POST /v1/turns`, `GET /v1/turns/{id}/activity`, `GET`/`PUT /v1/model`, `GET /v1/commands`). Creating a server binds no listener. The provider passes its own identity, engine name, build version and features. Callers receive only the displayable part of an `ExplainedFailure`; a request a closing runtime abandons (`RuntimeClosed`) is answered 503 without an explanation, which core reads as no answer; every other failure is logged, redacted, by method and path.
- [`failure.ts`](src/failure.ts): `ExplainedFailure` and `RuntimeClosed`, the classes the adapter recognises by `instanceof`, and the conservative, best-effort rules for what of an explanation may be shown and how diagnostics are redacted.
- [`activity.ts`](src/activity.ts): deny-by-default descriptions of what a tool acts on (the Bash command-header policy, workspace-relative paths, operation targets), the headline of a model's public text, and bounded single-line display. Native event tracking stays with each provider.
- [`installation.ts`](src/installation.ts): what a runtime service reads of the installation registry `carillon.toml`, its installation credential, and `--name value` arguments.
- [`endpoint.ts`](src/endpoint.ts): loopback listen-address validation.
- [`serial.ts`](src/serial.ts): one-at-a-time execution in arrival order.

Tests beside each module cover the behaviour callers depend on: failure display and redaction, command headers and tool descriptions, the HTTP boundary's authentication, activity and error handling, and endpoint validation.
