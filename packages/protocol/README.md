# Wire contracts

Rust uses [`carillon-protocol`](../../crates/carillon-protocol/src/lib.rs) and
TypeScript `@carillon/protocol` for the JSON core, interfaces and runtimes
exchange: turns, activity reports, jobs, model selection and commands, and
what a runtime says of the turns it holds: their outcomes, the turns
it opens and the controls relayed to it.
Providers own what option values mean; Pi, for example, translates
`thinking`. The Discord module's own invocation contract is
[`contract.rs`](../../crates/carillon-discord-module/src/contract.rs).

The [fixtures](fixtures) hold canonical and invalid examples that both
languages' tests must accept or refuse alike, so the two can't drift without
a failing test. `mise run check` runs them; there's no code generator. A
reset is an explicit `selection: null`, and optional fields may be omitted
but not sent as null.

`ModelState.providers` names each provider once with `{ id, name }`.
`ModelOption.choices` carries `{ id, name }` for each choice. Runtimes supply
these display names from their native catalogues, or the ID when no name
exists. A `Selection` holds IDs, including the choice ID in each option
value. For example, a picker shows Codex's **Speed: Fast** while its selection
sends `serviceTier: "priority"`.

Installation files and credentials belong to `carillon-installation`, so the
Wasm module can use the wire types without pulling in host filesystem code.
