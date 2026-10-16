import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ModelSelections } from "@carillon/runtime/selections";
import { expect, it } from "vitest";
import { agentModels, catalog, ledgerUpdate, modelState, type Catalog } from "./models.ts";

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
  const settings = { provider: "codex", model: "gpt-6-astra", options: {} };
  const models = agentModels(offered, { selection: settings, overridden: false }, settings);
  expect(modelState(models, undefined).providers).toEqual([
    { id: "carillon_claude", name: "Claude · Carillon" },
    { id: "codex", name: "codex" },
  ]);
});

it("names every choice of a setting as T3 labels it, and a boolean's as off or on", () => {
  const [codex] = catalog({
    providers: [
      {
        instanceId: "codex",
        enabled: true,
        installed: true,
        status: "ready",
        models: [
          {
            slug: "gpt-6-astra",
            name: "GPT-6 Astra",
            capabilities: {
              optionDescriptors: [
                {
                  id: "serviceTier",
                  label: "Service Tier",
                  type: "select",
                  options: [
                    { id: "default", label: "Standard", isDefault: true },
                    { id: "priority", label: "Fast" },
                    { id: "ultrafast", label: "Ultrafast" },
                  ],
                },
                { id: "fastMode", label: "Fast Mode", type: "boolean" },
              ],
            },
          },
        ],
      },
    ],
  }).available;
  expect(codex?.options).toEqual([
    {
      id: "serviceTier",
      name: "Service Tier",
      choices: [
        { id: "default", name: "Standard" },
        { id: "priority", name: "Fast" },
        { id: "ultrafast", name: "Ultrafast" },
      ],
      default: "default",
    },
    {
      id: "fastMode",
      name: "Fast Mode",
      choices: [
        { id: "off", name: "Off" },
        { id: "on", name: "On" },
      ],
      default: "off",
    },
  ]);
});

it("refuses a ledger change committed after reading a default set aside by a role change", async () => {
  const directory = await mkdtemp(join(tmpdir(), "carillon-t3-ledger-"));
  try {
    const path = join(directory, "models.json");
    const first = await ModelSelections.open(path);
    const other = await ModelSelections.open(path);
    const previous = { provider: "carillon_claude", model: "haiku", options: {} };
    const concurrent = { provider: "carillon_claude", model: "sonnet", options: {} };
    const settings = { provider: "codex", model: "gpt", options: {} };
    const available = [previous, concurrent, settings].map((selection) => ({
      ...selection,
      name: selection.model,
      options: [],
    }));
    const offered: Catalog = {
      providers: [{ id: "codex", name: "Codex" }],
      available: available.filter(({ provider }) => provider === "codex"),
      booleans: new Set(),
      runsOn: (provider) => provider === "codex",
    };
    const query = { interface_id: "discord", agent_id: "code" };
    await first.update(available, settings, { ...query, scope: "agent", selection: previous });
    const { agent: chosen } = await first.state(available, settings, query);
    const models = agentModels(offered, chosen, settings);
    expect(models.agent).toEqual({ selection: settings, overridden: false });
    const update = ledgerUpdate(models, {
      ...query,
      scope: "agent",
      selection: settings,
      expected: settings,
    });
    await other.update(available, settings, { ...query, scope: "agent", selection: concurrent });
    await expect(first.update(offered.available, settings, update)).rejects.toMatchObject({
      status: 409,
      message: "The model settings changed.",
    });
    expect((await first.state(available, settings, query)).agent.selection).toEqual(concurrent);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
