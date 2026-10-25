import { expect, it } from "vitest";
import {
  answer,
  conversation,
  dispatches,
  execute,
  fake,
  restart,
  runtime,
  selection,
  started,
  thread,
  threadId,
  turn,
} from "./harness.ts";

const agentDefault = {
  provider: "claudeAgent",
  model: "claude-sonnet-5",
  options: { effort: "low", fastMode: "off" },
};
const modelSets = () =>
  fake.commands.filter((command) => command.type === "thread.model-selection.set");

it("reads the conversation's model from its native thread against T3's catalogue", async () => {
  const state = await runtime.modelState(conversation);
  // Only usable provider instances are offered; a boolean setting is on or off.
  expect(state.available).toEqual([
    {
      provider: "claudeAgent",
      model: "claude-sonnet-5",
      name: "Claude Sonnet 5",
      options: [
        {
          id: "effort",
          name: "Effort",
          choices: [
            { id: "low", name: "Low" },
            { id: "high", name: "High" },
          ],
          default: "high",
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
      ],
    },
    { provider: "claudeAgent", model: "claude-haiku-4-5", name: "Claude Haiku 4.5", options: [] },
    { provider: "codex", model: "gpt-6-astra", name: "GPT-6 Astra", options: [] },
  ]);
  // The agent default is the settings' selection until `/model` changes it; a
  // conversation without a thread yet will start with it. Reading creates nothing.
  expect(state.agent).toEqual({ selection: agentDefault, overridden: false });
  expect(state.conversation).toEqual({ selection: agentDefault, overridden: false });
  expect(fake.threads.size).toBe(0);
  expect(
    await runtime.modelState({ interface_id: "discord-private", agent_id: "default" }),
  ).not.toHaveProperty("conversation");

  // A selection made in T3's own UI is the conversation's.
  const reply = execute(turn("turn-1", "hi"));
  answer(await started("turn-1"), "Hello.");
  await reply;
  thread().modelSelection = { instanceId: "claudeAgent", model: "claude-haiku-4-5", options: [] };
  expect((await runtime.modelState(conversation)).conversation).toEqual({
    selection: { provider: "claudeAgent", model: "claude-haiku-4-5", options: {} },
    overridden: true,
  });
});

it("changes the thread's own selection, which later messages inherit, and reads it back", async () => {
  // A change before the first message opens the thread with the agent default first.
  const haiku = { provider: "claudeAgent", model: "claude-haiku-4-5" };
  expect(
    (await runtime.setModel({ ...conversation, scope: "conversation", selection: haiku }))
      .conversation,
  ).toEqual({ selection: { ...haiku, options: {} }, overridden: true });
  expect(fake.commands.find((command) => command.type === "thread.create")).toMatchObject({
    modelSelection: { instanceId: "claudeAgent", model: "claude-sonnet-5" },
  });
  expect(thread().modelSelection).toEqual({
    instanceId: "claudeAgent",
    model: "claude-haiku-4-5",
    options: [],
  });

  const reply = execute(turn("turn-1", "Which model?"));
  answer(await started("turn-1"), "Haiku.");
  await reply;
  expect(dispatches()[0]).toMatchObject({
    modelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
  });
  expect(thread().modelSelection.model).toBe("claude-haiku-4-5");

  // A setting changes against the selection the panel showed; booleans reach T3 as booleans.
  const sonnet = await runtime.setModel({
    ...conversation,
    scope: "conversation",
    selection: { provider: "claudeAgent", model: "claude-sonnet-5" },
  });
  const shown = sonnet.conversation?.selection;
  expect(shown).toEqual({ ...agentDefault, options: { effort: "high", fastMode: "off" } });
  const fast = await runtime.setModel({
    ...conversation,
    scope: "conversation",
    selection: { ...agentDefault, options: { effort: "high", fastMode: "on" } },
    ...(shown == null ? {} : { expected: shown }),
  });
  expect(thread().modelSelection.options).toEqual([
    { id: "effort", value: "high" },
    { id: "fastMode", value: true },
  ]);
  expect(fast.conversation?.selection?.options).toEqual({ effort: "high", fastMode: "on" });

  // A change made in T3's own UI meanwhile makes the panel's view stale.
  thread().modelSelection = { instanceId: "claudeAgent", model: "claude-haiku-4-5", options: [] };
  await expect(
    runtime.setModel({
      ...conversation,
      scope: "conversation",
      selection: { ...agentDefault, options: { effort: "low" } },
      ...(fast.conversation?.selection == null ? {} : { expected: fast.conversation.selection }),
    }),
  ).rejects.toMatchObject({ status: 409, message: "The model settings changed." });
  expect(thread().modelSelection.model).toBe("claude-haiku-4-5");

  // A model T3's catalogue no longer lists is still the thread's selection,
  // so a view from before T3 switched to it is stale too.
  const shownHaiku = (await runtime.modelState(conversation)).conversation?.selection;
  thread().modelSelection = { instanceId: "claudeAgent", model: "claude-retired", options: [] };
  expect((await runtime.modelState(conversation)).conversation?.selection).toEqual({
    provider: "claudeAgent",
    model: "claude-retired",
    options: {},
  });
  await expect(
    runtime.setModel({
      ...conversation,
      scope: "conversation",
      selection: { ...agentDefault, options: { effort: "low" } },
      ...(shownHaiku == null ? {} : { expected: shownHaiku }),
    }),
  ).rejects.toMatchObject({ status: 409, message: "The model settings changed." });
  expect(thread().modelSelection.model).toBe("claude-retired");

  // Returning to the agent default sets it on the thread, every setting resolved.
  const reset = await runtime.setModel({ ...conversation, scope: "conversation", selection: null });
  expect(reset.conversation).toEqual({ selection: agentDefault, overridden: false });
  expect(thread().modelSelection).toEqual({
    ...selection,
    options: [...selection.options, { id: "fastMode", value: false }],
  });
});

it("reports no model when T3 no longer lists the thread whose model it changed", async () => {
  const haiku = { provider: "claudeAgent", model: "claude-haiku-4-5" };
  // T3 commits the change and answers with its receipt, then answers the
  // read that follows with its typed missing-projection error.
  let committed: unknown;
  fake.failAfterDispatch = (command) => {
    if (command.type === "thread.model-selection.set") {
      committed = structuredClone(thread().modelSelection);
      fake.threads.delete(threadId);
    }
    return undefined;
  };
  await expect(
    runtime.setModel({ ...conversation, scope: "conversation", selection: haiku }),
  ).rejects.toMatchObject({
    status: 502,
    message: "T3 answered in a form this runtime does not understand.",
  });
  // The change T3 committed is neither reported as the default nor sent again.
  expect(committed).toEqual({ instanceId: "claudeAgent", model: "claude-haiku-4-5", options: [] });
  expect(modelSets()).toHaveLength(1);
});

it("keeps the agent default `/model` chooses, which new threads start with", async () => {
  const haiku = { provider: "claudeAgent", model: "claude-haiku-4-5" };
  const channel = { interface_id: "discord-private", agent_id: "default" };
  const changed = await runtime.setModel({ ...channel, scope: "agent", selection: haiku });
  expect(changed.agent).toEqual({ selection: { ...haiku, options: {} }, overridden: true });
  // A new conversation's thread opens with it, and its message names it.
  const reply = execute(turn("turn-1", "hi"));
  answer(await started("turn-1"), "Hello.");
  await reply;
  expect(thread().modelSelection).toMatchObject({
    instanceId: "claudeAgent",
    model: "claude-haiku-4-5",
  });
  // The ledger keeps it: a restarted runtime reads it back.
  await restart();
  expect((await runtime.modelState(channel)).agent).toEqual({
    selection: { ...haiku, options: {} },
    overridden: true,
  });
  // An instance T3 does not offer is refused here too.
  await expect(
    runtime.setModel({
      ...channel,
      scope: "agent",
      selection: { provider: "cursor", model: "auto" },
    }),
  ).rejects.toMatchObject({ status: 422 });
  // Returning to the default restores the settings'.
  expect((await runtime.setModel({ ...channel, scope: "agent", selection: null })).agent).toEqual({
    selection: agentDefault,
    overridden: false,
  });
});

it("keeps the agent default `/model` chose while T3 no longer lists its model", async () => {
  const channel = { interface_id: "discord-private", agent_id: "default" };
  const haiku = { provider: "claudeAgent", model: "claude-haiku-4-5" };
  await runtime.setModel({ ...channel, scope: "agent", selection: haiku });
  // The default stays, and its run fails in T3 rather than another model running.
  const listing = (instanceId: string, slug: string) => ({
    instanceId,
    driver: instanceId,
    enabled: true,
    installed: true,
    status: "ready",
    models: [{ slug, name: slug, isCustom: false, capabilities: null }],
  });
  fake.providers = [listing("claudeAgent", "claude-sonnet-5"), listing("codex", "gpt-6-astra")];
  expect((await runtime.modelState(channel)).agent).toEqual({
    selection: { ...haiku, options: {} },
    overridden: true,
  });
});

it("refuses changes it cannot make, without sending them to T3", async () => {
  const change = (selection: {
    provider: string;
    model: string;
    options?: Record<string, string>;
  }) => runtime.setModel({ ...conversation, scope: "conversation", selection });
  // T3 accepts any model name or setting value, so the catalogue decides.
  await expect(change({ provider: "claudeAgent", model: "no-such-model" })).rejects.toMatchObject({
    status: 422,
    message: "That model is not available.",
  });
  await expect(change({ provider: "cursor", model: "auto" })).rejects.toMatchObject({
    status: 422,
  });
  await expect(
    change({ provider: "claudeAgent", model: "claude-sonnet-5", options: { effort: "max" } }),
  ).rejects.toMatchObject({ status: 422 });
  expect(modelSets()).toEqual([]);

  // While a turn of the conversation runs, or anything else works on its thread.
  const reply = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  await expect(
    change({ provider: "claudeAgent", model: "claude-haiku-4-5" }),
  ).rejects.toMatchObject({ status: 409 });
  answer(run, "Done.");
  await reply;
  fake.wake(threadId, "A run started in T3's own UI");
  await expect(
    change({ provider: "claudeAgent", model: "claude-haiku-4-5" }),
  ).rejects.toMatchObject({ status: 409 });
  expect(modelSets()).toEqual([]);
  const ui = thread().runs.at(-1);
  if (ui !== undefined) fake.complete(threadId, ui.id);

  // T3's own refusal is reported as such.
  fake.reject = (command) =>
    command.type === "thread.model-selection.set"
      ? "The target provider instance is unavailable."
      : undefined;
  const refused = change({ provider: "claudeAgent", model: "claude-haiku-4-5" });
  await expect(refused).rejects.toMatchObject({ status: 422 });
  await expect(refused).rejects.toThrow(
    /^T3 refused the model change: .*The target provider instance is unavailable\.$/u,
  );
  expect(thread().modelSelection).toEqual({
    ...selection,
    options: [...selection.options, { id: "fastMode", value: false }],
  });
});
