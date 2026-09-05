# Agent resources

A Carillon agent has a stable product identity, a selected runtime adapter, common human-readable instructions, a runtime-owned resource root, and a real user workspace.

For the Pi runtime adapter, Carillon's resource source remains an ordinary Pi-compatible directory:

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
- `settings.json`, `extensions/`, `skills/`, and `sessions/` retain their native Pi meanings. Provider authentication and engine settings are not agent resources; Pi persists them in `CARILLON_HOME/pi`, isolated from `~/.pi/agent`.

At dispatch, Carillon supplies an immutable snapshot of `SYSTEM.md`, `AGENTS.md`, `settings.json`, extensions, and skills as Pi's global `agentDir` resources. Separately, Pi's `cwd` is the configured real workspace—by default the owner's home. Native tools operate there and on ordinary absolute paths with the owner's authority. Pi-native project-local resources in an intentionally selected workspace retain their normal semantics.

The configured resource root must already be a directory. Core refuses new acceptance if it is missing or is not a directory; Pi refuses startup instead of recreating it. Individual resource files remain optional, so an intentionally empty root is valid. Known acceptance receipts and already captured snapshots do not depend on the live root remaining available.

Pi also validates that its selected default model is available with locally configured authentication before listening. Incomplete model setup leaves queued work queued, rather than claiming a turn merely to discover the setup error. This does not guarantee that a remote model service will accept a later request.

Pi composes the final prompt, including context, tool guidance, skill inventory, project resources, and extension changes. Engine settings remain Pi's responsibility.

Pi extensions use Pi's own loading, hooks, subprocess environment, tools, and persistence. Carillon does not introduce another extension manifest, permission system, or prompt-composition language.

A different runtime adapter may reuse the common instruction files where their meaning fits and own different native resources where it does not. Carillon core treats runtime resources and continuation references as adapter-owned rather than forcing Codex or another engine into Pi's settings, extension, or session format.

Operators edit common and runtime-native files with ordinary tools. A coherent change becomes available to subsequent turns while an existing turn either continues with the resources it began with or ends honestly.

The runtime adapter remains authoritative for model-visible history and native extension state. Carillon stores the product-level agent identity, selected provider/resource identity, and opaque continuation needed to connect runtime work to external conversations.
