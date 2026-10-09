# Bounded Wasm host

[`Host<N>`](src/lib.rs) hosts replaceable modules without knowing Discord, Pi, or
installation layout. The provider supplies a concrete `Native` implementation to
interpret byte requests. Construct a host, compile a module, then invoke it with
native operations and a deadline. Every invocation gets a fresh instance.

The ABI and limits are documented [at the public entry point](src/lib.rs). The
memory limit applies to guest linear memory, not the whole process. A deadline
cannot undo a native side effect that already happened.

For a production caller, see [Discord command hosting](../carillon-discord/src/commands.rs).
It owns contract validation, registration, replacement and the installed module:
failed candidates leave the last good generation serving, while in-flight requests
retain their generation.
Those are provider policies, not responsibilities of this host.

```sh
mise exec -- cargo test -p carillon-wasm
mise run test:integration
```

The host's local tests cover ABI validation, isolation, memory and deadlines.
The service integration suite exercises the compiled Discord module and
its install/reset path. A provider that does not need replaceable modules need not use Wasm.
