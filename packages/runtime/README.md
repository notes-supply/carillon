# Shared runtime boundary

`@carillon/runtime` is what every runtime provider serves to core, whatever
engine is behind it. It depends on `@carillon/protocol` and small parsing
libraries only. [Pi](../runtime-pi/README.md) and [T3](../runtime-t3/README.md)
depend on it; it imports neither. Import modules by path
(`@carillon/runtime/<module>`).

- [`operations.ts`](src/operations.ts): `RuntimeOperations`, the interface a
  runtime implements: run a turn, describe its activity, read and change the
  model selection, list commands, say whether a restart loses anything, close.
- [`http.ts`](src/http.ts): the authenticated HTTP adapter over those
  operations (`/health`, `POST /v1/turns`, `GET /v1/turns/{id}/activity`,
  `GET`/`PUT /v1/model`, `GET /v1/commands`, `GET /v1/restart`). Callers get
  only the displayable part of an explanation; other failures are logged,
  redacted.
- [`failure.ts`](src/failure.ts): `ExplainedFailure`, `RuntimeClosed` and the
  best-effort rules for what of an explanation may be shown.
- [`activity.ts`](src/activity.ts): deny-by-default descriptions of what a
  tool acts on, and `finalReport`, which keeps a turn's report within the
  protocol's size limit and marks it `partial` when it had to drop anything.
- [`installation.ts`](src/installation.ts), [`endpoint.ts`](src/endpoint.ts),
  [`serial.ts`](src/serial.ts): registry and credential loading, loopback
  address checks, and one-at-a-time execution.
