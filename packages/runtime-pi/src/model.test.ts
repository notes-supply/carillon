import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, it } from "vitest";
import { Check } from "typebox/value";
import { DETERMINISTIC_DEFAULT, DETERMINISTIC_MODELS, describeSelection } from "./testing.ts";
import { ModelStateSchema } from "@carillon/protocol";
import { ExplainedFailure } from "@carillon/runtime/failure";
import { resolved } from "@carillon/runtime/models";
import { ModelSelections } from "@carillon/runtime/selections";
import { type AvailableModel } from "@carillon/protocol";

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
    expect(
      await selections.state(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, conversation),
    ).toEqual({
      available: DETERMINISTIC_MODELS,
      agent: { selection: DETERMINISTIC_DEFAULT, overridden: false },
      conversation: { selection: DETERMINISTIC_DEFAULT, overridden: false },
    });
    expect(
      await selections.state(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
        interface_id: "discord-private",
        agent_id: "default",
      }),
    ).not.toHaveProperty("conversation");

    const agentSet = await selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
      ...conversation,
      scope: "agent",
      selection: reasoner,
    });
    expect(agentSet.agent).toEqual({ selection: reasoner, overridden: true });
    expect(agentSet.conversation).toEqual({ selection: reasoner, overridden: false });
    expect(
      await selections.effective(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, conversation),
    ).toEqual(reasoner);
    expect(
      await selections.effective(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, elsewhere),
    ).toEqual(reasoner);

    const fixed = { provider: "substitute", model: "fixed", options: {} };
    const conversationSet = await selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
      ...conversation,
      scope: "conversation",
      selection: fixed,
    });
    expect(conversationSet.agent).toEqual({ selection: reasoner, overridden: true });
    expect(conversationSet.conversation).toEqual({ selection: fixed, overridden: true });
    expect(
      await selections.effective(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, conversation),
    ).toEqual(fixed);
    expect(
      await selections.effective(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, elsewhere),
    ).toEqual(reasoner);

    // Returning a scope to its default removes only that scope's choice.
    const agentCleared = await selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
      ...conversation,
      scope: "agent",
      selection: null,
    });
    expect(agentCleared.agent).toEqual({ selection: DETERMINISTIC_DEFAULT, overridden: false });
    expect(agentCleared.conversation).toEqual({ selection: fixed, overridden: true });
    expect(
      await selections.effective(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, elsewhere),
    ).toEqual(DETERMINISTIC_DEFAULT);

    // Choices survive a restart through the ledger file alone.
    const reopened = await ModelSelections.open(path);
    expect(
      await reopened.effective(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, conversation),
    ).toEqual(fixed);
    expect(
      await reopened.effective(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, elsewhere),
    ).toEqual(DETERMINISTIC_DEFAULT);
  });
});

it("reports and runs the settings a choice leaves to the model's defaults", async () => {
  await withLedger(async (path) => {
    const selections = await ModelSelections.open(path);
    const chosen = { provider: "local", model: "reasoner", options: {} };
    const runs = { ...chosen, options: { thinking: "medium" } };
    const state = await selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
      ...conversation,
      scope: "agent",
      selection: chosen,
    });
    expect(state.agent).toEqual({ selection: runs, overridden: true });
    expect(
      await selections.effective(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, conversation),
    ).toEqual(runs);
    // The ledger keeps only what was chosen, so a changed default applies later.
    expect(JSON.parse(await readFile(path, "utf8"))).toMatchObject({
      agents: [{ agent_id: "default", selection: chosen }],
    });

    // A chosen value the catalogue stops supporting gives way to the default
    // in both the report and the run, while the choice itself stays recorded.
    await selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
      ...conversation,
      scope: "agent",
      selection: reasoner,
    });
    const narrowed: readonly AvailableModel[] = DETERMINISTIC_MODELS.map((model) =>
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
    expect((await selections.state(narrowed, DETERMINISTIC_DEFAULT, conversation)).agent).toEqual({
      selection: lowered,
      overridden: true,
    });
    expect(await selections.effective(narrowed, DETERMINISTIC_DEFAULT, conversation)).toEqual(
      lowered,
    );
    expect(
      await selections.effective(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, conversation),
    ).toEqual(reasoner);

    // A model the catalogue no longer offers is reported as chosen, not replaced.
    const without = DETERMINISTIC_MODELS.filter((model) => model.model !== "reasoner");
    expect(resolved(without, reasoner)).toEqual(reasoner);
    expect(
      (await selections.state(without, DETERMINISTIC_DEFAULT, conversation)).agent.selection,
    ).toEqual(reasoner);
  });
});

it("applies concurrent changes in order without losing any", async () => {
  await withLedger(async (path) => {
    const selections = await ModelSelections.open(path);
    const fixed = { provider: "substitute", model: "fixed", options: {} };
    const states = await Promise.all([
      selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
        ...conversation,
        scope: "agent",
        selection: reasoner,
      }),
      selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
        ...elsewhere,
        scope: "conversation",
        selection: fixed,
      }),
      selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
        ...conversation,
        scope: "agent",
        selection: DETERMINISTIC_DEFAULT,
      }),
    ]);
    expect(states.map((state) => state.agent.selection)).toEqual([
      reasoner,
      reasoner,
      DETERMINISTIC_DEFAULT,
    ]);
    const reopened = await ModelSelections.open(path);
    expect(
      await reopened.effective(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, elsewhere),
    ).toEqual(fixed);
    expect(
      (await reopened.state(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, conversation)).agent,
    ).toEqual({ selection: DETERMINISTIC_DEFAULT, overridden: true });
  });
});

it("checks stale-write preconditions inside the write queue for both scopes", async () => {
  await withLedger(async (path) => {
    const selections = await ModelSelections.open(path);
    for (const scope of ["agent", "conversation"] as const) {
      await selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
        ...conversation,
        scope,
        selection: reasoner,
      });
      const newer = { ...reasoner, options: { thinking: "low" } };
      const outcomes = await Promise.allSettled([
        selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
          ...conversation,
          scope,
          selection: newer,
          expected: reasoner,
        }),
        selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
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
        (await reopened.state(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, conversation))[scope]
          ?.selection,
      ).toEqual(newer);
    }
  });
});

it("a conversation option write cannot undo an inherited model change", async () => {
  await withLedger(async (path) => {
    const selections = await ModelSelections.open(path);
    await selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
      ...conversation,
      scope: "agent",
      selection: reasoner,
    });
    const expected = (
      await selections.state(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, conversation)
    ).conversation?.selection;
    if (!expected) throw new Error("fixture must have an inherited selection");
    await selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
      ...conversation,
      scope: "agent",
      selection: DETERMINISTIC_DEFAULT,
    });
    const before = await readFile(path, "utf8");
    await expect(
      selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
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
        selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
          ...conversation,
          scope: "agent",
          selection,
        }),
      ).rejects.toThrow(message);
    }
    await expect(
      selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
        interface_id: conversation.interface_id,
        agent_id: conversation.agent_id,
        scope: "conversation",
        selection: reasoner,
      }),
    ).rejects.toSatisfy(
      (failure: unknown) => failure instanceof ExplainedFailure && failure.status === 400,
    );
    await expect(readFile(path)).rejects.toMatchObject({ code: "ENOENT" });
    expect(
      await selections.effective(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, conversation),
    ).toEqual(DETERMINISTIC_DEFAULT);
  });
});

it("refuses a ledger it cannot read rather than starting empty", async () => {
  await withLedger(async (path) => {
    await writeFile(path, '{"agents": "not a list"}');
    await expect(ModelSelections.open(path)).rejects.toThrow();
  });
});

it("names selections with their options", () => {
  expect(describeSelection(DETERMINISTIC_DEFAULT)).toBe("local/echo");
  expect(describeSelection(reasoner)).toBe("local/reasoner (thinking=high)");
});

it("produces shared wire states including each model option default", async () => {
  await withLedger(async (path) => {
    const ledger = await ModelSelections.open(path);
    const state = await ledger.state(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, conversation);
    expect(Check(ModelStateSchema, state)).toBe(true);
    expect(
      state.available
        .flatMap((model) => model.options)
        .every((option) => option.choices.includes(option.default)),
    ).toBe(true);
  });
});

it("independent objects serialize file writes and see each other's committed state", async () => {
  await withLedger(async (path) => {
    const first = await ModelSelections.open(path);
    const second = await ModelSelections.open(path);
    await Promise.all([
      first.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
        ...conversation,
        agent_id: "one",
        scope: "agent",
        selection: reasoner,
      }),
      second.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
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
            await store.state(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
              ...conversation,
              agent_id,
            })
          ).agent.selection,
        ).toEqual(reasoner);
      }
    }
    const outcomes = await Promise.allSettled(
      [first, second].map((store) =>
        store.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, {
          ...conversation,
          agent_id: "one",
          scope: "agent",
          selection: DETERMINISTIC_DEFAULT,
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
      import { ModelSelections } from ${JSON.stringify(new URL("../../runtime/src/selections.ts", import.meta.url).href)};
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
          (await store.state([], DETERMINISTIC_DEFAULT, { interface_id: "discord", agent_id }))
            .agent.overridden,
        ).toBe(true);
      }
    } finally {
      for (const { child } of children) if (child.exitCode === null) child.kill();
    }
  });
});
