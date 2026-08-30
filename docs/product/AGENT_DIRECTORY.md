# Agent resources

A Carillon agent has a stable product identity, a selected runtime adapter, common human-readable instructions, and a runtime-owned resource root.

For the Pi runtime adapter, the resource root remains an ordinary Pi-compatible directory:

```text
<agent>/
├── SYSTEM.md
├── AGENTS.md
├── settings.json
├── extensions/
├── skills/
└── sessions/
```

- `SYSTEM.md` is the plainly named operator-authored system prompt.
- `AGENTS.md` contains optional contextual instructions.
- `settings.json`, `extensions/`, `skills/`, and `sessions/` retain their native Pi meanings. Provider authentication, discovered catalogs, and the service's selected model are not agent resources; Pi persists them in its private state directory.

The Pi adapter loads this directory explicitly rather than inheriting an operator's unrelated global environment. Pi composes the final prompt, including context, tool guidance, skill inventory, and extension changes. An explicit private model selection takes precedence over a model default in snapshotted agent settings.

Pi extensions use Pi's own hooks to append prompt material, register tools, contribute resources, and preserve extension state. Carillon does not introduce another extension manifest or prompt-composition language.

A different runtime adapter may reuse the common instruction files where their meaning fits and own different native resources where it does not. Carillon core treats runtime resources and continuation references as adapter-owned rather than forcing Codex or another engine into Pi's settings, extension, or session format.

Operators edit common and runtime-native files with ordinary tools. A coherent change becomes available to subsequent turns while an existing turn either continues with the resources it began with or ends honestly. Selected interface extensions and capabilities are associated through Carillon composition rather than embedded into the runtime's private file format.

The runtime adapter remains authoritative for model-visible history and native extension state. Carillon stores the product-level agent identity, selected component/resource identity, and opaque correlation needed to connect runtime work to external conversations.
