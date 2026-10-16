import { expect, it } from "vitest";
import { agentModels, catalog, modelState } from "./models.ts";

it("names each usable instance as T3 displays it, or by its ID when T3 gives no name", () => {
  const usable = { enabled: true, installed: true, status: "ready" };
  const offered = catalog({
    providers: [
      {
        ...usable,
        instanceId: "carillon_claude",
        displayName: "Claude · Carillon",
        models: [{ slug: "claude-opus-5-5", name: "Claude Opus 5.5" }],
      },
      { ...usable, instanceId: "codex", models: [{ slug: "gpt-6-astra", name: "GPT-6 Astra" }] },
      { ...usable, instanceId: "cursor", displayName: "Cursor", enabled: false, models: [] },
    ],
  });
  const models = agentModels(
    offered,
    { selection: null, overridden: false },
    { provider: "codex", model: "gpt-6-astra", options: {} },
  );
  expect(modelState(models, undefined).providers).toEqual([
    { id: "carillon_claude", name: "Claude · Carillon" },
    { id: "codex", name: "codex" },
  ]);
});
