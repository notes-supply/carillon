import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, it } from "vitest";
import { Check } from "typebox/value";
import { ModelStateSchema, type AvailableModel, type Selection } from "@carillon/protocol";
import { ExplainedFailure } from "./failure.ts";
import { resolved } from "./models.ts";
import { ModelSelections } from "./selections.ts";

const models: readonly AvailableModel[] = [
  { provider: "local", model: "echo", name: "Local echo", options: [] },
  {
    provider: "local",
    model: "reasoner",
    name: "Local reasoner",
    options: [
      {
        id: "thinking",
        name: "Thinking",
        choices: ["off", "low", "medium", "high"],
        default: "medium",
      },
    ],
  },
  { provider: "substitute", model: "fixed", name: "Substitute fixed", options: [] },
];
const runtimeDefault: Selection = { provider: "local", model: "echo", options: {} };
const conversation = {
  interface_id: "discord-private",
  agent_id: "default",
  audience_id: "discord-100-300",
};
const elsewhere = { ...conversation, audience_id: "discord-100-301" };
const reasoner = { provider: "local", model: "reasoner", options: { thinking: "high" } };

async function withLedger(run: (path: string) => Promise<void>): Promise<void> {
  const directory = await mkdtemp(join(tmpdir(), "carillon-model-"));
  try {
    await run(join(directory, "model-selection.json"));
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

it("reports runtime defaults until a scope chooses, then follows the nearest scope", async () => {
  await withLedger(async (path) => {
    const selections = await ModelSelections.open(path);
    expect(await selections.state(models, runtimeDefault, conversation)).toEqual({
      available: models,
      agent: { selection: runtimeDefault, overridden: false },
      conversation: { selection: runtimeDefault, overridden: false },
    });
    expect(
      await selections.state(models, runtimeDefault, {
        interface_id: "discord-private",
        agent_id: "default",
      }),
    ).not.toHaveProperty("conversation");

    const agentSet = await selections.update(models, runtimeDefault, {
      ...conversation,
      scope: "agent",
      selection: reasoner,
    });
    expect(agentSet.agent).toEqual({ selection: reasoner, overridden: true });
    expect(agentSet.conversation).toEqual({ selection: reasoner, overridden: false });
    expect(await selections.effective(models, runtimeDefault, conversation)).toEqual(reasoner);
    expect(await selections.effective(models, runtimeDefault, elsewhere)).toEqual(reasoner);

    const fixed = { provider: "substitute", model: "fixed", options: {} };
    const conversationSet = await selections.update(models, runtimeDefault, {
      ...conversation,
      scope: "conversation",
      selection: fixed,
    });
    expect(conversationSet.agent).toEqual({ selection: reasoner, overridden: true });
    expect(conversationSet.conversation).toEqual({ selection: fixed, overridden: true });
    expect(await selections.effective(models, runtimeDefault, conversation)).toEqual(fixed);
    expect(await selections.effective(models, runtimeDefault, elsewhere)).toEqual(reasoner);

    // Returning a scope to its default removes only that scope's choice.
    const agentCleared = await selections.update(models, runtimeDefault, {
      ...conversation,
      scope: "agent",
      selection: null,
    });
    expect(agentCleared.agent).toEqual({ selection: runtimeDefault, overridden: false });
    expect(agentCleared.conversation).toEqual({ selection: fixed, overridden: true });
    expect(await selections.effective(models, runtimeDefault, elsewhere)).toEqual(runtimeDefault);

    // Choices survive a restart through the ledger file alone.
    const reopened = await ModelSelections.open(path);
    expect(await reopened.effective(models, runtimeDefault, conversation)).toEqual(fixed);
    expect(await reopened.effective(models, runtimeDefault, elsewhere)).toEqual(runtimeDefault);
  });
});

it("reports and runs the settings a choice leaves to the model's defaults", async () => {
  await withLedger(async (path) => {
    const selections = await ModelSelections.open(path);
    const chosen = { provider: "local", model: "reasoner", options: {} };
    const runs = { ...chosen, options: { thinking: "medium" } };
    const state = await selections.update(models, runtimeDefault, {
      ...conversation,
      scope: "agent",
      selection: chosen,
    });
    expect(state.agent).toEqual({ selection: runs, overridden: true });
    expect(await selections.effective(models, runtimeDefault, conversation)).toEqual(runs);
    // The ledger keeps only what was chosen, so a changed default applies later.
    expect(JSON.parse(await readFile(path, "utf8"))).toMatchObject({
      agents: [{ agent_id: "default", selection: chosen }],
    });

    // A chosen value the catalogue stops supporting gives way to the default
    // in both the report and the run, while the choice itself stays recorded.
    await selections.update(models, runtimeDefault, {
      ...conversation,
      scope: "agent",
      selection: reasoner,
    });
    const narrowed: readonly AvailableModel[] = models.map((model) =>
      model.model === "reasoner"
        ? {
            ...model,
            options: [
              { id: "thinking", name: "Thinking", choices: ["off", "low"], default: "low" },
            ],
          }
        : model,
    );
    const lowered = { ...chosen, options: { thinking: "low" } };
    expect((await selections.state(narrowed, runtimeDefault, conversation)).agent).toEqual({
      selection: lowered,
      overridden: true,
    });
    expect(await selections.effective(narrowed, runtimeDefault, conversation)).toEqual(lowered);
    expect(await selections.effective(models, runtimeDefault, conversation)).toEqual(reasoner);

    // A model the catalogue no longer offers is reported as chosen, not replaced.
    const without = models.filter((model) => model.model !== "reasoner");
    expect(resolved(without, reasoner)).toEqual(reasoner);
    expect((await selections.state(without, runtimeDefault, conversation)).agent.selection).toEqual(
      reasoner,
    );
  });
});

it("applies concurrent changes in order without losing any", async () => {
  await withLedger(async (path) => {
    const selections = await ModelSelections.open(path);
    const fixed = { provider: "substitute", model: "fixed", options: {} };
    const states = await Promise.all([
      selections.update(models, runtimeDefault, {
        ...conversation,
        scope: "agent",
        selection: reasoner,
      }),
      selections.update(models, runtimeDefault, {
        ...elsewhere,
        scope: "conversation",
        selection: fixed,
      }),
      selections.update(models, runtimeDefault, {
        ...conversation,
        scope: "agent",
        selection: runtimeDefault,
      }),
    ]);
    expect(states.map((state) => state.agent.selection)).toEqual([
      reasoner,
      reasoner,
      runtimeDefault,
    ]);
    const reopened = await ModelSelections.open(path);
    expect(await reopened.effective(models, runtimeDefault, elsewhere)).toEqual(fixed);
    expect((await reopened.state(models, runtimeDefault, conversation)).agent).toEqual({
      selection: runtimeDefault,
      overridden: true,
    });
  });
});

it("checks stale-write preconditions inside the write queue for both scopes", async () => {
  await withLedger(async (path) => {
    const selections = await ModelSelections.open(path);
    for (const scope of ["agent", "conversation"] as const) {
      await selections.update(models, runtimeDefault, {
        ...conversation,
        scope,
        selection: reasoner,
      });
      const newer = { ...reasoner, options: { thinking: "low" } };
      const outcomes = await Promise.allSettled([
        selections.update(models, runtimeDefault, {
          ...conversation,
          scope,
          selection: newer,
          expected: reasoner,
        }),
        selections.update(models, runtimeDefault, {
          ...conversation,
          scope,
          selection: reasoner,
          expected: reasoner,
        }),
      ]);
      expect(outcomes[0].status).toBe("fulfilled");
      expect(outcomes[1]).toMatchObject({ status: "rejected", reason: { status: 409 } });
      const reopened = await ModelSelections.open(path);
      expect(
        (await reopened.state(models, runtimeDefault, conversation))[scope]?.selection,
      ).toEqual(newer);
    }
  });
});

it("a conversation option write cannot undo an inherited model change", async () => {
  await withLedger(async (path) => {
    const selections = await ModelSelections.open(path);
    await selections.update(models, runtimeDefault, {
      ...conversation,
      scope: "agent",
      selection: reasoner,
    });
    const expected = (await selections.state(models, runtimeDefault, conversation)).conversation
      ?.selection;
    if (!expected) throw new Error("fixture must have an inherited selection");
    await selections.update(models, runtimeDefault, {
      ...conversation,
      scope: "agent",
      selection: runtimeDefault,
    });
    const before = await readFile(path, "utf8");
    await expect(
      selections.update(models, runtimeDefault, {
        ...conversation,
        scope: "conversation",
        selection: reasoner,
        expected,
      }),
    ).rejects.toMatchObject({ status: 409 });
    expect(await readFile(path, "utf8")).toBe(before);
  });
});

it("refuses selections the catalog does not offer without touching the ledger", async () => {
  await withLedger(async (path) => {
    const selections = await ModelSelections.open(path);
    for (const [selection, message] of [
      [{ provider: "local", model: "missing", options: {} }, /not available/u],
      [{ provider: "local", model: "echo", options: { thinking: "high" } }, /no thinking setting/u],
      [{ provider: "local", model: "reasoner", options: { thinking: "max" } }, /not a Thinking/u],
    ] as const) {
      await expect(
        selections.update(models, runtimeDefault, {
          ...conversation,
          scope: "agent",
          selection,
        }),
      ).rejects.toThrow(message);
    }
    await expect(
      selections.update(models, runtimeDefault, {
        interface_id: conversation.interface_id,
        agent_id: conversation.agent_id,
        scope: "conversation",
        selection: reasoner,
      }),
    ).rejects.toSatisfy(
      (failure: unknown) => failure instanceof ExplainedFailure && failure.status === 400,
    );
    await expect(readFile(path)).rejects.toMatchObject({ code: "ENOENT" });
    expect(await selections.effective(models, runtimeDefault, conversation)).toEqual(
      runtimeDefault,
    );
  });
});

it("refuses a ledger it cannot read rather than starting empty", async () => {
  await withLedger(async (path) => {
    await writeFile(path, '{"agents": "not a list"}');
    await expect(ModelSelections.open(path)).rejects.toThrow();
  });
});

it("reports its state in the shared wire form", async () => {
  await withLedger(async (path) => {
    const ledger = await ModelSelections.open(path);
    await ledger.update(models, runtimeDefault, {
      ...conversation,
      scope: "conversation",
      selection: reasoner,
    });
    expect(Check(ModelStateSchema, await ledger.state(models, runtimeDefault, conversation))).toBe(
      true,
    );
  });
});

it("independent objects serialize file writes and see each other's committed state", async () => {
  await withLedger(async (path) => {
    const first = await ModelSelections.open(path);
    const second = await ModelSelections.open(path);
    await Promise.all([
      first.update(models, runtimeDefault, {
        ...conversation,
        agent_id: "one",
        scope: "agent",
        selection: reasoner,
      }),
      second.update(models, runtimeDefault, {
        ...conversation,
        agent_id: "two",
        scope: "agent",
        selection: reasoner,
      }),
    ]);
    for (const store of [first, second, await ModelSelections.open(path)]) {
      for (const agent_id of ["one", "two"]) {
        expect(
          (
            await store.state(models, runtimeDefault, {
              ...conversation,
              agent_id,
            })
          ).agent.selection,
        ).toEqual(reasoner);
      }
    }
    const outcomes = await Promise.allSettled(
      [first, second].map((store) =>
        store.update(models, runtimeDefault, {
          ...conversation,
          agent_id: "one",
          scope: "agent",
          selection: runtimeDefault,
          expected: reasoner,
        }),
      ),
    );
    expect(outcomes.filter((result) => result.status === "fulfilled")).toHaveLength(1);
    expect(outcomes.filter((result) => result.status === "rejected")).toMatchObject([
      { reason: { status: 409 } },
    ]);
  });
});

it("serializes selection updates across processes", async () => {
  await withLedger(async (path) => {
    const script = `
      import { ModelSelections } from ${JSON.stringify(new URL("./selections.ts", import.meta.url).href)};
      const [path, agent] = process.argv.slice(1);
      const store = await ModelSelections.open(path);
      process.on("message", async () => {
        await store.update([{provider:"test", model:"small", name:"Small", options:[]}], {provider:"test", model:"small", options:{}},
          {interface_id:"discord", agent_id:agent, scope:"agent", selection:{provider:"test", model:"small", options:{}}});
        process.disconnect();
      });
      process.send("ready");
    `;
    const children = ["first", "second"].map((agent) => {
      const child = spawn(
        process.execPath,
        ["--input-type=module", "--eval", script, path, agent],
        {
          stdio: ["ignore", "ignore", "inherit", "ipc"],
        },
      );
      return { child, ready: once(child, "message"), exited: once(child, "exit") };
    });
    try {
      await Promise.all(children.map(({ ready }) => ready));
      for (const { child } of children) child.send("write");
      expect(await Promise.all(children.map(({ exited }) => exited))).toEqual([
        [0, null],
        [0, null],
      ]);
      const store = await ModelSelections.open(path);
      for (const agent_id of ["first", "second"]) {
        expect(
          (await store.state([], runtimeDefault, { interface_id: "discord", agent_id })).agent
            .overridden,
        ).toBe(true);
      }
    } finally {
      for (const { child } of children) if (child.exitCode === null) child.kill();
    }
  });
});
