# Wire contracts

Rust uses [`carillon-protocol`](../../crates/carillon-protocol/src/lib.rs) and
TypeScript `@carillon/protocol` for the JSON core, interfaces and runtimes
exchange: turns, activity reports, jobs, settings and commands, and
what a runtime says of the turns it holds: their outcomes, the turns
it opens and the controls relayed to it.
Owners declare their settings and what each value means; Pi, for example,
translates `thinking`. The Discord module's own invocation contract is
[`contract.rs`](../../crates/carillon-discord-module/src/contract.rs).

The [fixtures](fixtures) hold canonical and invalid examples that both
languages' tests must accept or refuse alike, so the two can't drift without
a failing test. `mise run check` runs them; there's no code generator. A
reset is an explicit `value: null`, and optional fields may be omitted but
not sent as null.

A settings `Page` is about one `Subject`, an agent or a routed channel. Each
`Setting` carries its choices as `{ id, name, group }`, its value as choice
IDs, and its owner's `Revision`, which a `Change` must name. Owners supply
display names from their native catalogues, or the ID when no name exists.
For example, a page shows Codex's **Speed** as **Fast** while the change
sends the choice ID `priority`. Only Rust sends and reads a `Page`: the
Discord provider serves it and core's CLI reads it. Runtimes exchange
`SettingsRequest` and `RuntimeSettings`, which TypeScript mirrors.

Installation files and credentials belong to `carillon-installation`, so the
Wasm module can use the wire types without pulling in host filesystem code.
