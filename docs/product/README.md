# Carillon

Carillon is a durable self-hosted composition service for agents. It connects independently supervised interface adapters, runtime adapters, interface extensions, and capabilities while keeping identity, routing, continuity, and recovery coherent.

Pi is the first agent engine behind a runtime adapter. Discord is the first interface behind an interface adapter. Neither defines the permanent core model.

The planning corpus is intentionally small:

- [PRODUCT.md](PRODUCT.md) defines the product, user journey, and required behavior.
- [ARCHITECTURE.md](ARCHITECTURE.md) describes the top-level system and ownership boundaries.
- [AGENT_DIRECTORY.md](AGENT_DIRECTORY.md) describes common agent files and runtime-owned resources.
- [OPERATIONS.md](OPERATIONS.md) describes installation, live extension, and recovery.

`PRODUCT.md` is the primary brief. The other documents explain boundaries that materially affect the product; they are not a prescribed implementation sequence.

Implementation agents choose their own work breakdown, intermediate steps, internal APIs, tests, and libraries. The architecture establishes responsibility and extensibility, not a prescribed framework or workflow. Exact dependencies and low-level decisions belong in the repository as the implementation takes shape.
