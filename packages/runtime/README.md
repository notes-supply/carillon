# Shared runtime boundary

`@carillon/runtime` is what every runtime provider serves to core, whatever
engine is behind it. It depends on `@carillon/protocol` and small parsing
libraries only. [Pi](../runtime-pi/README.md) and [T3](../runtime-t3/README.md)
depend on it; it imports neither. Import modules by path
(`@carillon/runtime/<module>`).

- [`operations.ts`](src/operations.ts): `RuntimeOperations`, the interface a
  runtime implements: hold turns, describe their activity, read and change
  its settings, list commands, say whether a restart loses anything,
  close. Its `turns` `submit` keeps a turn for the runtime to settle later
  through core, and `control` acts on one a person controls.
- [`http.ts`](src/http.ts): the authenticated HTTP adapter over those
  operations (`/health`, `POST /v1/inbox`, answering 202 once kept, `POST
  /v1/turns/{id}/control`, `GET /v1/turns/{id}/activity`, `POST
  /v1/settings`, `GET /v1/commands` and `GET /v1/restart`). Callers get only
  the displayable part of an explanation; other failures are logged,
  redacted.
- [`core.ts`](src/core.ts): a runtime's client of core, with its own
  credential: `settle` a turn it holds, in one attempt the caller repeats
  until core says what it made of the outcome, `open` one of its own, and
  list the turns core records `running` on it, for a process that forgot
  them, setting apart an entry it cannot read. [`testing.ts`](src/testing.ts) is a
  small fake core for runtime tests.
- [`failure.ts`](src/failure.ts): `ExplainedFailure`, `RuntimeClosed` and the
  best-effort rules for what of an explanation may be shown.
- [`activity.ts`](src/activity.ts): deny-by-default descriptions of what a
  tool acts on, and `finalReport`, which keeps a turn's report within the
  protocol's size limit and marks it `partial` when it had to drop anything.
- [`installation.ts`](src/installation.ts), [`endpoint.ts`](src/endpoint.ts),
  [`serial.ts`](src/serial.ts): registry and credential loading, including
  core's loopback address; loopback address checks; and one-at-a-time
  execution, which a waiter can leave.
- [`settings.ts`](src/settings.ts): `SettingsLedger`, the model unit each
  agent and channel stores in the runtime's `settings.json`, their revisions,
  and what a turn in a channel runs.
- [`publish.ts`](src/publish.ts): durable replacement of a file a runtime
  owns, such as its settings ledger.
