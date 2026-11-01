import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, it } from "vitest";
import type { Setting, Subject } from "@carillon/protocol";
import { ExplainedFailure } from "./failure.ts";
import { noModel, SettingsLedger, type Catalogue } from "./settings.ts";

const catalogue: Catalogue = {
  providers: [{ id: "local", name: "Local" }],
  models: [
    { provider: "local", model: "echo", name: "Local echo", options: [] },
    {
      provider: "local",
      model: "reasoner",
      name: "Local reasoner",
      options: [
        {
          id: "thinking",
          name: "Thinking",
          choices: ["low", "medium", "high"].map((id) => ({ id, name: id })),
          default: "medium",
        },
      ],
    },
    {
      provider: "local",
      model: "deliberate",
      name: "Local deliberate",
      options: [
        {
          id: "thinking",
          name: "Thinking",
          choices: ["medium", "high"].map((id) => ({ id, name: id })),
          default: "high",
        },
      ],
    },
  ],
};
const agent = { kind: "agent", agent_id: "assistant" } as const satisfies Subject;
const home = {
  kind: "channel",
  interface_id: "discord-private",
  channel_id: "300",
  agent_id: "assistant",
} as const satisfies Subject;
const other = { ...home, channel_id: "301" } as const satisfies Subject;

async function withLedger(run: (ledger: SettingsLedger, path: string) => Promise<void>) {
  const directory = await mkdtemp(join(tmpdir(), "carillon-settings-"));
  try {
    const path = join(directory, "settings.json");
    await run(await SettingsLedger.open(path), path);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

function line(lines: readonly Setting[], id: string): Setting {
  const found = lines.find((candidate) => candidate.id === id);
  if (found === undefined) throw new Error(`no ${id} line`);
  return found;
}

async function choose(ledger: SettingsLedger, subject: Subject, setting: string, id: string) {
  const current = line(await ledger.settings(catalogue, subject), setting);
  return ledger.change(catalogue, subject, {
    setting,
    value: { kind: "one", id },
    expected: current.revision,
  });
}

async function refusal(promise: Promise<unknown>): Promise<number | undefined> {
  try {
    await promise;
    return undefined;
  } catch (failure) {
    if (failure instanceof ExplainedFailure) return failure.status;
    throw failure;
  }
}

it("shows no model and resolves none until the agent has a value", async () => {
  await withLedger(async (ledger) => {
    const lines = await ledger.settings(catalogue, home);
    expect(lines).toHaveLength(1);
    expect(line(lines, "model")).toMatchObject({ value: null, inherited: true });
    const channel = { interface_id: "discord-private", channel_id: "300", agent_id: "assistant" };
    expect(await ledger.selection(catalogue, channel)).toBeUndefined();
    expect(noModel("assistant")).toContain("carillon settings --agent assistant model=");
  });
});

it("lets a channel inherit the agent value until it chooses its own", async () => {
  await withLedger(async (ledger) => {
    await choose(ledger, agent, "model", "local/reasoner");
    const inherited = await ledger.settings(catalogue, home);
    expect(line(inherited, "model")).toMatchObject({
      value: { kind: "one", id: "local/reasoner" },
      inherited: true,
    });
    expect(line(inherited, "model.thinking")).toMatchObject({
      value: { kind: "one", id: "medium" },
      inherited: true,
    });

    await choose(ledger, home, "model.thinking", "high");
    expect(line(await ledger.settings(catalogue, home), "model.thinking")).toMatchObject({
      value: { kind: "one", id: "high" },
      inherited: false,
    });
    expect(line(await ledger.settings(catalogue, other), "model.thinking")).toMatchObject({
      value: { kind: "one", id: "medium" },
      inherited: true,
    });
    const channel = { interface_id: "discord-private", channel_id: "300", agent_id: "assistant" };
    expect(await ledger.selection(catalogue, channel)).toEqual({
      provider: "local",
      model: "reasoner",
      options: { thinking: "high" },
    });
  });
});

it("gives two agents that inherit the same default different revisions", async () => {
  await withLedger(async (ledger) => {
    await choose(ledger, agent, "model", "local/reasoner");
    await choose(ledger, { kind: "agent", agent_id: "helper" }, "model", "local/reasoner");
    const mine = line(await ledger.settings(catalogue, home), "model.thinking");
    const theirs = line(
      await ledger.settings(catalogue, { ...home, agent_id: "helper" }),
      "model.thinking",
    );
    expect(mine.value).toEqual(theirs.value);
    expect(mine.revision).not.toBe(theirs.revision);
    expect(
      await refusal(
        ledger.change(
          catalogue,
          { ...home, agent_id: "helper" },
          {
            setting: "model.thinking",
            value: { kind: "one", id: "high" },
            expected: mine.revision,
          },
        ),
      ),
    ).toBe(409);
  });
});

it("replaces the unit when the model changes and keeps the model when an option changes", async () => {
  await withLedger(async (ledger, path) => {
    await choose(ledger, home, "model", "local/reasoner");
    await choose(ledger, home, "model.thinking", "low");
    expect(line(await ledger.settings(catalogue, home), "model")).toMatchObject({
      value: { kind: "one", id: "local/reasoner" },
    });

    const lines = await choose(ledger, home, "model", "local/deliberate");
    expect(line(lines, "model.thinking").value).toEqual({ kind: "one", id: "high" });
    expect(JSON.parse(await readFile(path, "utf8"))).toEqual({
      agents: [],
      channels: [
        {
          interface_id: "discord-private",
          channel_id: "300",
          agent_id: "assistant",
          model: { provider: "local", model: "deliberate", options: { thinking: "high" } },
        },
      ],
    });
  });
});

it("refuses a stale option edit after the model changed under it", async () => {
  await withLedger(async (ledger) => {
    await choose(ledger, home, "model", "local/reasoner");
    const drawn = line(await ledger.settings(catalogue, home), "model.thinking");
    await choose(ledger, home, "model", "local/deliberate");
    expect(
      await refusal(
        ledger.change(catalogue, home, {
          setting: "model.thinking",
          value: { kind: "one", id: "medium" },
          expected: drawn.revision,
        }),
      ),
    ).toBe(409);
    expect(line(await ledger.settings(catalogue, home), "model.thinking").value).toEqual({
      kind: "one",
      id: "high",
    });
  });
});

it("refuses to reset a value pinned after the reset was drawn", async () => {
  await withLedger(async (ledger, path) => {
    await choose(ledger, agent, "model", "local/reasoner");
    const drawn = line(await ledger.settings(catalogue, home), "model");
    await choose(ledger, home, "model", "local/reasoner");
    const pinned = await readFile(path, "utf8");
    expect(
      await refusal(
        ledger.change(catalogue, home, { setting: "model", value: null, expected: drawn.revision }),
      ),
    ).toBe(409);
    expect(await readFile(path, "utf8")).toBe(pinned);

    const current = line(await ledger.settings(catalogue, home), "model");
    const reset = await ledger.change(catalogue, home, {
      setting: "model",
      value: null,
      expected: current.revision,
    });
    expect(line(reset, "model").inherited).toBe(true);
  });
});

it("refuses a value the catalogue does not offer", async () => {
  await withLedger(async (ledger) => {
    expect(await refusal(choose(ledger, home, "model", "local/missing"))).toBe(422);
    await choose(ledger, home, "model", "local/reasoner");
    expect(await refusal(choose(ledger, home, "model.thinking", "extreme"))).toBe(422);
  });
});

it("serializes changes from separately opened ledgers on one file", async () => {
  await withLedger(async (first, path) => {
    const second = await SettingsLedger.open(path);
    const drawn = line(await first.settings(catalogue, home), "model");
    const outcomes = await Promise.allSettled(
      ["local/reasoner", "local/deliberate"].map((id, index) =>
        (index === 0 ? first : second).change(catalogue, home, {
          setting: "model",
          value: { kind: "one", id },
          expected: drawn.revision,
        }),
      ),
    );
    expect(outcomes.filter(({ status }) => status === "fulfilled")).toHaveLength(1);
    const lost = outcomes.flatMap((outcome) =>
      outcome.status === "rejected" ? [outcome.reason] : [],
    );
    expect(lost).toHaveLength(1);
    expect(lost[0]).toBeInstanceOf(ExplainedFailure);
    expect((lost[0] as ExplainedFailure).status).toBe(409);
  });
});
