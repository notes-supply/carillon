import { ExplainedFailure } from "@carillon/runtime/failure";
import { Check } from "typebox/value";
import { expect, it } from "vitest";
import { derivedId, ThreadSnapshotSchema } from "../contracts.ts";
import { connectT3 } from "../t3.ts";
import { startFakeT3, type FakeT3 } from "../fake.ts";
import {
  execute,
  fake,
  openings,
  PROJECT,
  resources,
  restart,
  runtime,
  settled,
  turn as request,
  TurnFailed,
} from "./harness.ts";

const conversation = {
  interface_id: "discord-channel",
  agent_id: "default",
  audience_id: "channel-1",
};
const turn = (correlation: string, input: string, extra: Record<string, unknown> = {}) =>
  request(correlation, input, { ...conversation, ...extra });

const NEXT_PROJECT = "project-2";
const refusal = {
  status: 409,
  message:
    "This conversation ran in a T3 project this agent no longer works in. Your next message starts a new conversation in its current project.",
};

function projectThread(agentId = "default", projectId = NEXT_PROJECT) {
  const id = derivedId(
    "thread",
    projectId,
    conversation.interface_id,
    agentId,
    conversation.audience_id,
  );
  const thread = fake.threads.get(id);
  if (thread === undefined) throw new Error("the project's conversation thread does not exist");
  return thread;
}

async function started(correlation: string, server: FakeT3 = fake) {
  const find = () =>
    [...server.threads.values()].find((thread) =>
      thread.runs.some((run) => run.userMessageId === derivedId("message", correlation)),
    );
  await server.waitFor(() => find() !== undefined);
  const thread = find();
  const run = thread?.runs.find((run) => run.userMessageId === derivedId("message", correlation));
  if (thread === undefined || run === undefined) throw new Error("turn did not start");
  return { thread, run };
}

async function seed(agentId = "default") {
  const response = execute(turn("first", "Remember this project.", { agent_id: agentId }));
  const { thread, run } = await started("first");
  fake.assistant(thread.id, run.id, "Remembered.");
  fake.complete(thread.id, run.id);
  await response;
  return thread;
}

async function refused(operation: Promise<unknown>) {
  await expect(operation).rejects.toBeInstanceOf(ExplainedFailure);
  await expect(operation).rejects.toMatchObject(refusal);
}

/** A turn the runtime settled failed, explaining the refusal. */
async function turnRefused(turn: Promise<unknown>) {
  await expect(turn).rejects.toBeInstanceOf(TurnFailed);
  await expect(turn).rejects.toThrow(refusal.message);
}

it.each(["default", "code"])(
  "refuses one %s channel message on a project change, then continues there and resumes the original project",
  async (agentId) => {
    const old = await seed(agentId);
    await restart({ projectId: NEXT_PROJECT });
    const received = fake.received.length;
    await turnRefused(
      execute(
        turn("queued", "An already queued request.", { agent_id: agentId }),
        AbortSignal.timeout(500),
      ),
    );
    const next = projectThread(agentId);
    expect(next.projectId).toBe(NEXT_PROJECT);
    expect(next.messages).toEqual([]);
    expect(next.runs).toEqual([]);
    expect(fake.received.slice(received)).toEqual([
      expect.objectContaining({
        type: "thread.create",
        threadId: next.id,
        projectId: NEXT_PROJECT,
        title: "New thread",
      }),
    ]);
    expect(fake.threads.size).toBe(2);
    expect(old.messages.filter((message) => message.role === "user")).toHaveLength(1);
    expect(runtime.activity("queued", conversation.interface_id)).toBeUndefined();

    for (const correlation of ["second", "later"]) {
      await restart({ projectId: NEXT_PROJECT });
      const response = execute(turn(correlation, "Use the new project.", { agent_id: agentId }));
      const fresh = await started(correlation);
      expect(fresh.thread.id).toBe(next.id);
      await fake.waitFor(
        () => runtime.activity(correlation, conversation.interface_id)?.phase === "model",
      );
      fake.assistant(fresh.thread.id, fresh.run.id, "New project.");
      fake.complete(fresh.thread.id, fresh.run.id);
      expect((await response).output).toBe("New project.");
    }
    expect(fake.threads.size).toBe(2);
    expect(old.projectId).toBe(PROJECT);
    expect(old.messages.filter((message) => message.role === "user")).toHaveLength(1);
    expect(next.messages.filter((message) => message.role === "user")).toHaveLength(2);
    const sent = fake.commands.filter((command) => command.type === "message.dispatch");
    expect(sent.slice(1).map((command) => command.threadId)).toEqual([next.id, next.id]);
    expect(sent[1]?.["titleSeed"]).toBe("Use the new project.");

    await restart({ projectId: PROJECT });
    const resumed = execute(turn("resumed", "Back to the original.", { agent_id: agentId }));
    const original = await started("resumed");
    expect(original.thread.id).toBe(old.id);
    fake.assistant(old.id, original.run.id, "Resumed.");
    fake.complete(old.id, original.run.id);
    expect((await resumed).output).toBe("Resumed.");
    expect(fake.threads.size).toBe(2);
  },
);

it("keeps the native thread identity independent of the project", async () => {
  const old = await seed();
  const other = await startFakeT3();
  try {
    await restart({ url: other.url, projectId: NEXT_PROJECT });
    const response = execute(turn("next", "Start again."));
    const next = await started("next", other);
    other.assistant(next.thread.id, next.run.id, "Started.");
    other.complete(next.thread.id, next.run.id);
    await response;
    expect(next.thread.projectId).toBe(NEXT_PROJECT);
    expect(next.thread.id).toBe(old.id);
  } finally {
    await runtime.close();
    await other.close();
  }
});

it.each([
  "model read",
  "conversation model",
  "conversation reset",
  "agent model",
  "agent reset",
  "commands",
])(
  "refuses a bound %s request until a message creates the current project's thread, then uses it",
  async (operation) => {
    const old = await seed();
    await restart({ projectId: NEXT_PROJECT });
    const received = fake.received.length;
    const defaultQuery = {
      interface_id: conversation.interface_id,
      agent_id: conversation.agent_id,
    };
    const before = await runtime.modelState(defaultQuery);
    const selection = { provider: "claudeAgent", model: "claude-haiku-4-5" };
    const perform = () =>
      operation === "model read"
        ? runtime.modelState(conversation)
        : operation === "commands"
          ? runtime.commands(conversation)
          : runtime.setModel({
              ...conversation,
              scope: operation.startsWith("agent") ? "agent" : "conversation",
              selection: operation.endsWith("reset") ? null : selection,
            });
    await refused(perform());
    expect(fake.received).toHaveLength(received);
    expect(fake.refreshes).toEqual([]);
    expect(fake.threads.size).toBe(1);
    expect(old.modelSelection.model).toBe("claude-sonnet-5");
    expect(await runtime.modelState(defaultQuery)).toEqual(before);

    await turnRefused(execute(turn("cutover", "Start in the current project.")));
    const next = projectThread();
    next.modelSelection = { instanceId: "claudeAgent", model: "claude-haiku-4-5", options: [] };
    const commands = fake.received.length;
    await perform();
    expect(fake.threads.size).toBe(2);
    expect(next.messages).toEqual([]);
    expect(old.modelSelection.model).toBe("claude-sonnet-5");
    expect(fake.received.slice(commands).every((command) => command.threadId === next.id)).toBe(
      true,
    );
    expect(fake.refreshes).toEqual(
      operation === "commands" ? [{ instanceId: "claudeAgent", cwd: resources }] : [],
    );
    const state = await runtime.modelState(conversation);
    expect(state.conversation?.selection?.model).toBe(
      operation === "conversation reset" ? "claude-sonnet-5" : "claude-haiku-4-5",
    );
    expect(state.agent.selection?.model).toBe(
      operation === "agent model" ? "claude-haiku-4-5" : "claude-sonnet-5",
    );
    const response = execute(turn("selected", "Use this model."));
    const selected = await started("selected");
    expect(selected.thread.id).toBe(next.id);
    expect(selected.thread.modelSelection.model).toBe(state.conversation?.selection?.model);
    fake.assistant(next.id, selected.run.id, "Selected.");
    fake.complete(next.id, selected.run.id);
    await response;
    await restart({ projectId: PROJECT });
    expect((await runtime.modelState(conversation)).conversation?.selection?.model).toBe(
      "claude-sonnet-5",
    );
  },
);

it.each(["default", "code"])(
  "refuses %s native commands without creating a thread, then dispatches them in the current project's thread",
  async (agentId) => {
    await seed(agentId);
    await restart({ projectId: NEXT_PROJECT });
    const received = fake.received.length;
    await turnRefused(
      execute(
        turn("command", "/compact", { agent_id: agentId, native: { intent: "command" } }),
        AbortSignal.timeout(500),
      ),
    );
    expect(fake.received).toHaveLength(received);
    expect(fake.threads.size).toBe(1);

    await turnRefused(execute(turn("cutover", "Move projects.", { agent_id: agentId })));
    const next = projectThread(agentId);
    const response = execute(
      turn("next-command", "/compact", { agent_id: agentId, native: { intent: "command" } }),
    );
    const command = await started("next-command");
    expect(command.thread.id).toBe(next.id);
    expect(fake.commands.at(-1)).toMatchObject({
      type: "message.dispatch",
      threadId: next.id,
      text: "/compact",
    });
    fake.complete(next.id, command.run.id);
    await response;
    expect(fake.threads.size).toBe(2);
  },
);

it("refuses a report of delegated work from the previous project without stopping that work", async () => {
  const response = execute(turn("first", "Delegate the work."));
  const { thread, run } = await started("first");
  const task = fake.delegate(thread.id, run.id, { title: "Old project work" });
  fake.assistant(thread.id, run.id, "Started.");
  fake.complete(thread.id, run.id);
  await response;
  // The report's turn is taken up again after the restart, in the agent's new project.
  const received = fake.received.length;
  await restart({ projectId: NEXT_PROJECT });
  const wake = fake.finishDelegation(thread.id, task.id);
  fake.assistant(thread.id, wake.id, "Old work finished.");
  fake.complete(thread.id, wake.id);
  await turnRefused(settled("opened-1"));
  expect(fake.received).toHaveLength(received);
  expect(fake.interrupts).toEqual([]);
  expect(fake.detached).toEqual([]);
  expect(fake.threads.size).toBe(1);
  expect(runtime.activity("opened-1", conversation.interface_id)).toBeUndefined();

  await turnRefused(execute(turn("cutover", "Move projects.")));
  const next = projectThread();
  const currentResponse = execute(turn("current", "Delegate in this project."));
  const current = await started("current");
  const currentTask = fake.delegate(next.id, current.run.id, { title: "Current project work" });
  fake.assistant(next.id, current.run.id, "Delegated.");
  fake.complete(next.id, current.run.id);
  await currentResponse;
  const currentWake = fake.finishDelegation(next.id, currentTask.id);
  fake.assistant(next.id, currentWake.id, "Current work finished.");
  fake.complete(next.id, currentWake.id);
  expect((await settled("opened-2")).output).toBe("Current work finished.");
  expect(openings()).toHaveLength(2);
});

it("checks the watched snapshot before dispatch or activity even when the ownership read passed", async () => {
  await seed();
  let snapshots = 0;
  await restart({
    projectId: NEXT_PROJECT,
    connect: async (endpoint, options) => {
      const connection = await connectT3(endpoint, options);
      return {
        ...connection,
        subscribe: (tag, payload, onItem) =>
          connection.subscribe(tag, payload, (item) => {
            if (Check(ThreadSnapshotSchema, item) && ++snapshots <= 2)
              onItem({
                ...item,
                projection: {
                  ...item.projection,
                  thread: { ...item.projection.thread, projectId: NEXT_PROJECT },
                },
              });
            else onItem(item);
          }),
      };
    },
  });
  const received = fake.received.length;
  await turnRefused(execute(turn("watched", "Old work."), AbortSignal.timeout(500)));
  expect(snapshots).toBe(3);
  expect(fake.received).toHaveLength(received);
  expect(fake.threads.size).toBe(1);
  expect(runtime.activity("watched", conversation.interface_id)).toBeUndefined();
});

it("refuses thread records without a valid project before exposing their models", async () => {
  const old = await seed();
  fake.threads.set(old.id, { ...old, projectId: "" });
  const received = fake.received.length;
  await expect(runtime.modelState(conversation)).rejects.toMatchObject({
    status: 502,
    message: "T3 answered in a form this runtime does not understand.",
  });
  expect(fake.received).toHaveLength(received);
});
