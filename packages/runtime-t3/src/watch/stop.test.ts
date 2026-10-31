import { expect, it } from "vitest";
import { watchTurn } from "../watch.ts";
import { run, snapshot, event } from "./fixtures.ts";

const providerThread = (pendingBackgroundTasks: readonly { taskId: string; kind: string }[]) => ({
  id: "provider-thread",
  providerSessionId: "session",
  pendingBackgroundTasks,
});
const listed = [{ taskId: "t", kind: "command" }];

it("counts a stop T3 committed only once an event after it ends the listing of the turn's work", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("completed")], providerThreads: [providerThread(listed)] }));
  watch.apply(event(2, "provider-thread.updated", providerThread([])));
  // The process owes its wake: a clear at or before the stop's own commit is no evidence of it.
  expect([watch.quiet(), watch.quiet(3), watch.quiet(2), watch.quiet(1)]).toEqual([
    false,
    false,
    false,
    true,
  ]);
});

it("never dates the end of a listing only a snapshot shows, so the stop stays unconfirmed", () => {
  const watch = watchTurn("thread", "message-1", "/workspace");
  watch.apply(snapshot({ runs: [run("completed")], providerThreads: [providerThread(listed)] }));
  // The subscription resumes after a stop committed at 2, and the snapshot no longer lists the work.
  watch.apply(snapshot({ runs: [run("completed")], providerThreads: [providerThread([])] }, 3));
  expect(watch.outcome()).toBeUndefined();
  expect(watch.quiet(2)).toBe(false);
});
