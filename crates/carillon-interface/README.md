# Shared interface boundary

`carillon-interface` is what an interface provider does with core and the
installation, whatever platform is behind it. The
[Discord provider](../carillon-discord/README.md) depends on it; it knows no
platform. A provider keeps its own server, routes and loops. Wire types stay
in `carillon-protocol`.

- [`start`](src/start.rs): joins the installation. A provider the registry
  does not list, or lists as disabled, gets `None` and ends successfully,
  before it reads its credential or touches state, so its service is not
  restarted. Otherwise the credential is checked against the registry, then
  the address is bound, which makes the process the entry's only instance.
  `Started` carries the listener, the core client and what `GET /health`
  answers; the provider serves it.
- [`client`](src/client.rs): `CoreClient`, the provider's authenticated
  requests to core, with bounded waits: submit an event, read its status,
  jobs, controls, channel settings and commands. A successful control has no
  body. `Deliveries` is the claim, acknowledgement and reconciliation of
  deliveries under one instance identity. Its owner keeps two rules, which
  the type does not enforce, so that each delivery is sent once. A process
  opens one, because a reconcile under another identity turns the first
  one's live claims uncertain. The owner claims, sends and acknowledges in
  sequence, because until a claim's outcome is acknowledged core answers
  every claim under that identity with the same delivery.
- [`error`](src/error.rs): `InterfaceError`, a status and a message, which
  is also the JSON answer a provider's own routes give. `is_transient` says
  which failures are worth repeating: 5xx other than 502 and 507, and
  transport failures.
- [`submission`](src/submission.rs): `submit_event_with_retry` repeats a
  submission under the same event ID until core accepts or refuses it, and
  `settled` asks core what it holds of an event whose answer was lost.
- [`staging`](src/staging.rs): `Staging` is the provider's `incoming`
  directory, cleared at start. Each request stages its files in a fresh
  `Batch`, which removes them when dropped.
- [`operator`](src/operator.rs): `authorize_operator` checks a bearer
  against the registry's operator credential hash.

The tests in [`tests/interface.rs`](tests/interface.rs) run the crate against
an in-process fake of core's interface routes.
