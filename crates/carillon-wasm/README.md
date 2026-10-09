# Bounded Wasm host

[`Host<N>`](src/lib.rs) runs replaceable WebAssembly modules without knowing
Discord, Pi or the installation. The embedding provider supplies a `Native`
implementation that interprets the module's byte requests. Every invocation
gets a fresh instance, a cap on linear memory (not on the whole process) and a
deadline; a deadline can't undo a native side effect that already happened.
The ABI and limits are documented at [the entry point](src/lib.rs).

The Discord provider's [`module`](../carillon-discord/src/module.rs) is the
production caller: it owns contract validation, registration and replacement,
and keeps the last good module serving when a candidate fails.

```sh
mise exec -- cargo test -p carillon-wasm
```
