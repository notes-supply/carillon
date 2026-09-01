# Carillon

Carillon is a self-hosted personal agent system designed to be extended over time, including by the agents that use it.

It connects interface providers to agent runtime providers through a core service. The current implementation uses Discord as its interface and Pi as its runtime, but neither is fixed. Other interfaces, runtimes, extensions, and capabilities can be added as separate providers.

This separation lets an agent help add or replace parts of its own system without turning the project into a monolith. Providers can be developed, tested, deployed, and removed independently. The core routes requests and stores turn and delivery state.

## Current implementation

Carillon currently runs as three services:

- `carillon-core` routes requests and stores state.
- `carillon-discord` handles Discord messages.
- `carillon-runtime-pi` runs agent turns through Pi.

These are the providers included today, not a fixed list of supported components.

All services run as the same ordinary Unix user. Carillon uses that user's existing permissions instead of adding its own permission system.

## Status

Carillon is in early development and is currently only used by its developers. It is not ready for general use, and upgrade compatibility is not guaranteed.

The current deployment targets Linux with systemd.

## Development

The repository uses [Mise](https://mise.jdx.dev/):

```text
mise install
mise run check
mise run journey
mise run package
```

`mise run journey` tests the complete system locally. `mise run package` builds the Linux release archive.

## Documentation

- [Product](docs/product/PRODUCT.md)
- [Architecture](docs/product/ARCHITECTURE.md)
- [Operations](docs/product/OPERATIONS.md)

## License

Carillon is licensed under the [Apache License 2.0](LICENSE).
