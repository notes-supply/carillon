import { expect, it } from "vitest";
import { answer, dispatches, execute, fake, started, thread, turn } from "./harness.ts";

it("keeps a conversation's turns in one T3 thread and sends the person's text as written", async () => {
  const first = execute(turn("turn-1", "Remember carillon."));
  answer(await started("turn-1"), "Noted.");
  expect(await first).toMatchObject({ output: "Noted." });

  const second = execute(turn("turn-2", "Which word?"));
  answer(await started("turn-2"), "Carillon.");
  expect((await second).output).toBe("Carillon.");

  expect(fake.threads.size).toBe(1);
  expect(thread().runs.map((run) => run.ordinal)).toEqual([1, 2]);
  expect(
    thread()
      .messages.filter((message) => message.role === "user")
      .map((message) => message.text),
  ).toEqual(["Remember carillon.", "Which word?"]);
  // The thread was opened with the agent default; each message names the thread's own
  // selection, which the agent offers, so its run takes it.
  expect(fake.commands.find((command) => command.type === "thread.create")).toMatchObject({
    modelSelection: { instanceId: "claudeAgent", model: "claude-sonnet-5" },
  });
  for (const dispatch of dispatches())
    expect(dispatch["modelSelection"]).toEqual(thread().modelSelection);
  expect(fake.commands.filter((command) => command.type === "thread.create")).toHaveLength(1);
  for (const subscription of fake.subscriptions)
    expect(subscription).toMatchObject({ bounded: true, marker: true });
});
