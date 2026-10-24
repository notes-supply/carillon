# Shared runtime boundary

`@carillon/runtime` is what every runtime provider serves to core, whatever
engine is behind it. It depends on `@carillon/protocol` and small parsing
libraries only. [Pi](../runtime-pi/README.md) and [T3](../runtime-t3/README.md)
depend on it; it imports neither. Import modules by path
(`@carillon/runtime/<module>`).

- [`operations.ts`](src/operations.ts): `RuntimeOperations`, the interface a
  runtime implements: handle turns, describe their activity, read and change
  the model selection, list commands, say whether a restart loses anything,
  close. `turns` is one of two modes. In `answer` mode, T3's until it holds
  its turns, `execute` runs a turn while core waits. In `inbox` mode,
  `submit` keeps a turn for the runtime to settle later through core, and
  `control` acts on one a person controls.
- [`http.ts`](src/http.ts): the authenticated HTTP adapter over those
  operations (`/health`, `GET /v1/turns/{id}/activity`, `GET`/`PUT
  /v1/model`, `GET /v1/commands`, `GET /v1/restart`, and the mode's turn
  routes: `POST /v1/turns`, or `POST /v1/inbox`, answering 202 once kept, and
  `POST /v1/turns/{id}/control`). It refuses to serve a mode the registry
  entry's `durable` does not declare. Callers get only the displayable part
  of an explanation; other failures are logged, redacted.
- [`core.ts`](src/core.ts): an inbox runtime's client of core, with its own
  credential: `settle` a turn it holds (retrying core's failures and
  restarts) and `open` one of its own. [`testing.ts`](src/testing.ts) is a
  small fake core for runtime tests.
- [`failure.ts`](src/failure.ts): `ExplainedFailure`, `RuntimeClosed` and the
  best-effort rules for what of an explanation may be shown.
- [`activity.ts`](src/activity.ts): deny-by-default descriptions of what a
  tool acts on, and `finalReport`, which keeps a turn's report within the
  protocol's size limit and marks it `partial` when it had to drop anything.
- [`installation.ts`](src/installation.ts), [`endpoint.ts`](src/endpoint.ts),
  [`serial.ts`](src/serial.ts): registry and credential loading, including
  core's loopback address and the runtime's `durable`, loopback address
  checks, and one-at-a-time execution.
