import { expect, it } from "vitest";
import {
  answer,
  execute,
  fake,
  runOf,
  runtime,
  started,
  thread,
  threadId,
  turn,
  UNCONFIRMED,
} from "./harness.ts";

/** A turn whose completed run left `sleep 300 &` running and listed in the background. */
async function leftInBackground() {
  const controller = new AbortController();
  const result = execute(turn("turn-1", "work"), controller.signal);
  const run = await started("turn-1");
  const call = fake.tool(threadId, run.id, { type: "command_execution", input: "sleep 300 &" });
  fake.background(threadId, [{ taskId: "task-1", kind: "command", description: "sleep 300" }]);
  fake.assistant(threadId, run.id, "Running it in the background.");
  fake.complete(threadId, run.id);
  await fake.waitFor(
    () => activity()?.phase === "tools" && runOf("turn-1")?.status === "completed",
  );
  return {
    result,
    run,
    call: () => thread().items.find((item) => item.id === call.id)?.status,
    cancel: () => controller.abort(new Error("core disconnected")),
  };
}

const activity = () => runtime.activity("turn-1", "discord-private");

it("confirms the stop of background work a completed run left once T3 ends it after the interrupt", async () => {
  const { result, run, call, cancel } = await leftInBackground();
  cancel();
  await expect(result).rejects.toThrow("core disconnected");
  // T3 ends the work and keeps the session, which the next run reuses.
  const roster = thread().providerThread.pendingBackgroundTasks;
  expect([fake.interrupts, roster, call(), thread().session.status]).toEqual([
    [run.id],
    [],
    "interrupted",
    "ready",
  ]);
  expect(runtime.restartReadiness()).toEqual({ state: "ready" });

  const next = execute(turn("turn-2", "again"));
  answer(await started("turn-2"), "ok");
  expect((await next).output).toBe("ok");
  expect(fake.interrupts).toEqual([run.id]);
});

it.each([
  ["settle", "interrupted"],
  ["ignore", "running"],
] as const)(
  "leaves the stop unconfirmed when the work ended on its own before T3 committed it (interrupt: %s)",
  async (behaviour, status) => {
    fake.interruptBehaviour = behaviour;
    const { result, run, call, cancel } = await leftInBackground();
    // The roster empties on its own, so a wake is owed; T3 still accepts the
    // interrupt for the call left running.
    fake.background(threadId, []);
    await fake.waitFor(() => activity()?.running.length === 1);
    cancel();
    await expect(result).rejects.toThrow(UNCONFIRMED);
    expect([fake.interrupts, call()]).toEqual([[run.id], status]);
    expect(runtime.restartReadiness()).toMatchObject({ state: "unready" });
  },
);

/**
 * A turn whose running parent started `count` native children while T3
 * ignores interrupts. Past the fold's bound, the last child is known only as
 * live work left out.
 */
async function nativeChildren(count: number) {
  fake.interruptBehaviour = "ignore";
  const controller = new AbortController();
  const result = execute(turn("turn-1", "start native children"), controller.signal);
  const parent = await started("turn-1");
  const children = Array.from({ length: count }, (_, index) => ({
    id: `native-${String(index)}`,
    runId: parent.id,
    parentNodeId: `node:run:${parent.id}:root`,
    origin: "provider_native" as const,
    driver: "claudeAgent",
    status: "running" as const,
    title: `Native child ${String(index)}`,
    model: "claude-sonnet-5",
    childThreadId: `child-${String(index)}`,
    startedAt: "2026-10-31T00:00:00Z",
    completionWake: "always" as const,
  }));
  const calls = children.map((child) => {
    fake.subagent(threadId, child);
    return fake.tool(threadId, parent.id, {
      type: "subagent",
      subagentId: child.id,
      input: child.title,
      status: "running",
    });
  });
  fake.assistant(threadId, parent.id, "Children are running.");
  await fake.waitFor(() => activity()?.work?.items.some((item) => item.id === "native-0") === true);
  return {
    result,
    complete: () => fake.complete(threadId, parent.id),
    stop: async () => {
      controller.abort(new Error("core disconnected"));
      await fake.waitFor(() => fake.interrupts.includes(parent.id));
    },
    /** Every child but the last finishes on its own. */
    finishAllButLast: () => {
      for (const [index, child] of children.slice(0, -1).entries()) {
        fake.subagent(threadId, { ...child, status: "completed" });
        const call = calls[index];
        if (call !== undefined) fake.finishTool(threadId, call);
      }
    },
  };
}

it.each([2, 33])(
  "leaves an ignored stop unconfirmed while one of %s native children still runs",
  async (count) => {
    const children = await nativeChildren(count);
    children.complete();
    await children.stop();
    children.finishAllButLast();
    await expect(children.result).rejects.toThrow(UNCONFIRMED);
    expect(runtime.restartReadiness()).toMatchObject({ state: "unready" });
  },
);

it("leaves an ignored stop unconfirmed when the parent completes after all but a child past the bound", async () => {
  const children = await nativeChildren(33);
  await children.stop();
  children.finishAllButLast();
  children.complete();
  await expect(children.result).rejects.toThrow(UNCONFIRMED);
  expect(runtime.restartReadiness()).toMatchObject({ state: "unready" });
});

it("leaves an ignored stop unconfirmed after a resumed snapshot omits a child past the bound", async () => {
  fake.snapshotOnResume = true;
  const children = await nativeChildren(33);
  fake.background(threadId, [{ taskId: "task-1", kind: "command", description: "sleep 300" }]);
  children.complete();
  await fake.waitFor(() => activity()?.phase === "tools");
  children.finishAllButLast();
  // The resumed snapshot leaves out the child the fold never retained.
  fake.omitSubagent = (task) => task.id === "native-32";
  fake.disconnectAll();
  await children.stop();
  fake.background(threadId, []);
  await expect(children.result).rejects.toThrow(UNCONFIRMED);
  expect(runtime.restartReadiness()).toMatchObject({ state: "unready" });
});
