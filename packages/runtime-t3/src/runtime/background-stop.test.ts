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
