---
name: carillon-hooks
description: Write a Carillon extension whose hooks guard tool calls, log answers or change model requests.
disable-model-invocation: true
---

# Carillon hooks

A user extension is one module in the agent's resource root: `extensions/<file>.ts`, `.js` or `.mjs`, with no comma in the name. For the agent that setup creates, `assistant`, the root is `$CARILLON_HOME/agents/default/`, by default `~/.carillon/agents/default/`. For another agent, it is the `resources` directory of its `[[agents]]` entry in `$CARILLON_HOME/carillon.toml`.

- The default export is a pi-durable extension object without a name: `hooks`, and optionally `tools`, `sections`, `wraps` and `tasks`. Its settings name, such as `extensions/guard.ts`, is its name.
- It runs only once selected. Add it to the root's `settings.json`, keeping the default extensions: `{ "extensions": { "add": ["extensions/guard.ts"] } }`. `carillon settings --agent <agent> extensions=…` sets the whole list instead.
- It cannot import packages. Import only Node's built-ins, such as `node:fs`. Type annotations are fine, and so is `import type`, which is erased.
- Core copies the root with every message, and the next message loads the module. Nothing restarts. The module then applies to every request the agent makes, also for an answer already running.
- A module that does not load or install refuses every message, naming why, until it is fixed or deselected. Write it in full before you select it. `carillon settings --agent <agent> reset extensions` returns to the defaults.

## Hooks

A hook registration names a task and its handlers: `{ task: "pi.tool", handlers: { beforeTool(call, api, context) { … } } }`. A handler that returns `undefined` changes nothing. `api` reads committed documents and history (`api.entries(…)`, `api.entry(id, …)`) and keeps memos (`api.memo(name, context)`).

`pi.tool`, for every tool call:

- `beforeTool(call, api, context)`: `call` has `name`, `arguments` and `id`. Return `{ block: "reason" }` to refuse the call, whose result then says why, or `{ arguments }` to replace its arguments. A throw also refuses it.
- `afterTool(call, result, api, context)`: return a replacement for the result before it is committed.

`pi.generation`, for every model request of a run:

- `beforeRequest({ messages }, api, context)`: return `{ messages }` to send other messages in this request only.
- `beforeProviderRequest({ model, payload }, api, context)`: return `{ payload }` to replace the provider's request body. The body is in `model.api`'s format, so check `model.api` first.
- `afterResponse(message, api, context)`: every provider response as it ends, with `provider`, `model`, `stopReason` and `usage`. Returns nothing.
- `onYield(answer, api, context)`: return `{ continue: "text" }` to add a user message and keep the run going.
- `afterTools(assistant, results, api, context)`: after a round of tool calls, with the entry IDs of the call and its results.
- `afterRun(run, api, context)`: after a run answered. `run` has `reason` and the entry IDs `first`, `answer` and `last`.

`pi.compaction` has `beforeCompact(compaction, api, context)`, which may return `{ decline: true }` or `{ summary }`.

## Examples

Refuse force pushes. The guard matches words, and does not parse shell. It splits a bash command into parts at `;`, `&`, `|` and line breaks, and each part into words at spaces. It refuses a part whose first word is `git` when a word after the first `push` is `--force`, a group of short options with `f` such as `-f` or `-fu`, or starts with `+`. That `push` may come anywhere after `git`, so `git stash push -f` is refused too. Quotes and aliases are not read, so `git push "-f"` and `git push origin '+main'` pass, and so do `--force-with-lease` and a push run through `sudo` or a script:

```ts
export default {
  hooks: [
    {
      task: "pi.tool",
      handlers: {
        beforeTool(call) {
          if (call.name !== "bash") return undefined;
          for (const part of String(call.arguments.command ?? "").split(/[;&|\n]/u)) {
            const words = part.trim().split(/\s+/u);
            const push = words.indexOf("push");
            const forced = words
              .slice(push + 1)
              .some((word) => word === "--force" || /^-\w*f/u.test(word) || word.startsWith("+"));
            if (words[0] === "git" && push > 0 && forced)
              return { block: "Force pushes are off for this agent." };
          }
          return undefined;
        },
      },
    },
  ],
};
```

Log each model response to `carillon logs runtime-pi`:

```ts
export default {
  hooks: [
    {
      task: "pi.generation",
      handlers: {
        afterResponse(message) {
          console.error(
            `${message.provider}/${message.model}: ${message.stopReason}, ${String(message.usage.totalTokens)} tokens`,
          );
        },
      },
    },
  ],
};
```

Ask ChatGPT subscription models for longer answers:

```ts
export default {
  hooks: [
    {
      task: "pi.generation",
      handlers: {
        beforeProviderRequest({ model, payload }) {
          if (model.api !== "openai-codex-responses") return undefined;
          return { payload: { ...payload, text: { ...payload.text, verbosity: "high" } } };
        },
      },
    },
  ],
};
```
