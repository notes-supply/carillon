import { ExplainedFailure } from "@carillon/runtime/failure";
import type { TurnResponse } from "@carillon/runtime/operations";
import { expect, it } from "vitest";
import { derivedId } from "../contracts.ts";
import { conversation, fake, PROJECT, restart, runtime, turn } from "./harness.ts";

const NEXT_PROJECT = "project-2";
const refusal = {
  status: 409,
  message: `This conversation ran in T3 project ${PROJECT}, which is no longer this agent's project. Start a new conversation.`,
};

async function started(correlation: string) {
  const find = () =>
    [...fake.threads.values()].find((thread) =>
      thread.runs.some((run) => run.userMessageId === derivedId("message", correlation)),
    );
  await fake.waitFor(() => find() !== undefined);
  const thread = find();
  const run = thread?.runs.find((run) => run.userMessageId === derivedId("message", correlation));
  if (thread === undefined || run === undefined) throw new Error("turn did not start");
  return { thread, run };
}

async function seed() {
  const response = runtime.executeTurn(turn("first", "Remember this project."));
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

it("ends old conversations on a project change and starts new conversations in the new project", async () => {
  const old = await seed();
  await restart({ projectId: NEXT_PROJECT });
  const received = fake.received.length;
  await refused(
    runtime.executeTurn(turn("queued", "An already queued request."), AbortSignal.timeout(500)),
  );
  expect(fake.received).toHaveLength(received);
  expect(fake.threads.size).toBe(1);
  expect(old.messages.filter((message) => message.role === "user")).toHaveLength(1);
  expect(runtime.activity("queued", conversation.interface_id)).toBeUndefined();

  const response = runtime.executeTurn(
    turn("fresh", "Use the new project.", { audience_id: "new-conversation" }),
  );
  const fresh = await started("fresh");
  expect(fresh.thread.projectId).toBe(NEXT_PROJECT);
  fake.assistant(fresh.thread.id, fresh.run.id, "New project.");
  fake.complete(fresh.thread.id, fresh.run.id);
  expect((await response).output).toBe("New project.");
  expect(fake.threads.size).toBe(2);
  expect(old.projectId).toBe(PROJECT);
});

it("keeps the native thread identity independent of the project", async () => {
  const old = await seed();
  fake.threads.delete(old.id);
  await restart({ projectId: NEXT_PROJECT });
  const response = runtime.executeTurn(turn("next", "Start again."));
  const next = await started("next");
  fake.assistant(next.thread.id, next.run.id, "Started.");
  fake.complete(next.thread.id, next.run.id);
  await response;
  expect(next.thread.projectId).toBe(NEXT_PROJECT);
  expect(next.thread.id).toBe(old.id);
});

it.each(["model read", "conversation model", "conversation reset", "agent model", "commands"])(
  "refuses a bound %s request after a project change without changing native or default settings",
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
    const request =
      operation === "model read"
        ? runtime.modelState(conversation)
        : operation === "commands"
          ? runtime.commands(conversation)
          : runtime.setModel({
              ...conversation,
              scope: operation === "agent model" ? "agent" : "conversation",
              selection: operation === "conversation reset" ? null : selection,
            });
    await refused(request);
    expect(fake.received).toHaveLength(received);
    expect(fake.refreshes).toEqual([]);
    expect(fake.threads.size).toBe(1);
    expect(old.modelSelection.model).toBe("claude-sonnet-5");
    expect(await runtime.modelState(defaultQuery)).toEqual(before);
  },
);

it("refuses native commands before dispatch, including thread creation or title seeds", async () => {
  await seed();
  await restart({ projectId: NEXT_PROJECT });
  const received = fake.received.length;
  await refused(
    runtime.executeTurn(
      turn("command", "/compact", { native: { intent: "command" } }),
      AbortSignal.timeout(500),
    ),
  );
  expect(fake.received).toHaveLength(received);
  expect(fake.threads.size).toBe(1);
});

it("refuses observations of delegated work from the previous project without stopping that work", async () => {
  const response = runtime.executeTurn(turn("first", "Delegate the work."));
  const { thread, run } = await started("first");
  const task = fake.delegate(thread.id, run.id, { title: "Old project work" });
  fake.assistant(thread.id, run.id, "Started.");
  fake.complete(thread.id, run.id);
  const initial: TurnResponse = await response;
  expect(initial.follow_up).toBeDefined();
  await restart({ projectId: NEXT_PROJECT });
  const wake = fake.finishDelegation(thread.id, task.id);
  fake.assistant(thread.id, wake.id, "Old work finished.");
  fake.complete(thread.id, wake.id);
  const received = fake.received.length;
  await refused(runtime.executeTurn(turn("observation", "", { follow_up: initial.follow_up })));
  expect(fake.received).toHaveLength(received);
  expect(fake.interrupts).toEqual([]);
  expect(fake.detached).toEqual([]);
  expect(fake.threads.size).toBe(1);
  expect(runtime.activity("observation", conversation.interface_id)).toBeUndefined();
});
