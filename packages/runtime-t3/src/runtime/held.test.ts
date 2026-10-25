import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { ExplainedFailure } from "@carillon/runtime/failure";
import { expect, it, vi } from "vitest";
import { readStops } from "../held.ts";
import {
  answer,
  core,
  dispatches,
  execute,
  fake,
  openings,
  restart,
  runOf,
  runtime,
  settled,
  started,
  stopsFile,
  threadId,
  turn,
} from "./harness.ts";

/** What core received settling `turnId`. */
const settles = (turnId: string) =>
  core.received.filter(({ path }) => path === `/v1/runtime/turns/${turnId}/outcome`);

/** The calls core received from `from` on, as method and path. */
const calls = (from = 0) =>
  core.received.slice(from).map(({ method, path }) => `${method} ${path}`);

/** How the runtime answers a person's stop of `turnId`. */
const stopStatus = (turnId: string) =>
  runtime.turns.control(turnId, "discord-private", "stop").then(
    () => 204,
    (failure: unknown) => (failure instanceof ExplainedFailure ? failure.status : undefined),
  );

/** Hands over turn-1, which runs, and turn-2, which waits behind it in the runtime. */
async function oneWaiting() {
  const first = execute(turn("turn-1", "long work"));
  const long = await started("turn-1");
  const waiting = execute(turn("turn-2", "next"));
  // A test drives the runtime before it asserts how the stopped turn ended.
  void waiting.catch(() => undefined);
  await fake.waitFor(() => runtime.activity("turn-2", "discord-private") !== undefined);
  return { first, long, waiting };
}

/** Runs turn-1 until it delegates work to a child and answers. */
async function delegating() {
  const first = execute(turn("turn-1", "delegate it"));
  const parent = await started("turn-1");
  const child = fake.delegate(threadId, parent.id, { title: "child" });
  return { first, parent, child };
}

it("holds a turn core hands over twice as one, and settles it once", async () => {
  const request = turn("turn-1", "hello");
  const reply = execute(request);
  await runtime.turns.submit(request);
  answer(await started("turn-1"), "hi");
  expect((await reply).output).toBe("hi");
  expect(dispatches()).toHaveLength(1);
  const settles = core.received.filter(({ path }) => path === "/v1/runtime/turns/turn-1/outcome");
  expect(settles).toHaveLength(1);
  expect(settles[0]?.body).toMatchObject({ outcome: "answered", output: "hi" });
});

it("asks core again until it records the outcome", async () => {
  const reply = execute(turn("turn-1", "hello"));
  const run = await started("turn-1");
  core.answer(503, 500);
  answer(run, "hi");
  expect((await reply).output).toBe("hi");
  expect(
    core.received.filter(({ path }) => path === "/v1/runtime/turns/turn-1/outcome"),
  ).toHaveLength(3);
});

it("stops a message waiting behind its conversation's turn at once, leaving that turn running", async () => {
  const first = execute(turn("turn-1", "long work"));
  const long = await started("turn-1");
  const waiting = execute(turn("turn-2", "next"));
  // It waits in the runtime, offering only a stop.
  await fake.waitFor(() => runtime.activity("turn-2", "discord-private") !== undefined);
  expect(runtime.activity("turn-2", "discord-private")).toEqual({
    phase: "queued",
    running: [],
    recent: [],
    tool_calls: 0,
    controls: ["stop"],
  });
  for (const action of ["steer", "interrupt"] as const)
    await expect(runtime.turns.control("turn-2", "discord-private", action)).rejects.toMatchObject({
      status: 409,
    });
  await expect(runtime.turns.control("turn-2", "other-interface", "stop")).rejects.toMatchObject({
    status: 404,
  });
  await runtime.turns.control("turn-2", "discord-private", "stop");
  expect(await core.outcome("turn-2")).toEqual({ outcome: "stopped" });
  await expect(waiting).rejects.toThrow("the turn was stopped");
  // Nothing of it reached T3, and the turn before it runs on.
  expect(runOf("turn-2")).toBeUndefined();
  expect(fake.interrupts).toEqual([]);
  answer(long, "Long done.");
  expect((await first).output).toBe("Long done.");
  expect(dispatches()).toHaveLength(1);
  await expect(runtime.turns.control("turn-2", "discord-private", "stop")).rejects.toMatchObject({
    status: 404,
  });
});

it("stops a running message by interrupting its own run, once T3 confirms it", async () => {
  const reply = execute(turn("turn-1", "work"));
  const run = await started("turn-1");
  await runtime.turns.control("turn-1", "discord-private", "stop");
  expect(await core.outcome("turn-1")).toEqual({ outcome: "stopped" });
  await expect(reply).rejects.toThrow("the turn was stopped");
  expect(fake.interrupts).toEqual([run.id]);
  expect(runOf("turn-1")?.status).toBe("interrupted");
});

it("settles a stop T3 confirms while the runtime closes, and is not ready to restart until then", async () => {
  fake.interruptBehaviour = "ignore";
  const reply = execute(turn("turn-1", "work"));
  void reply.catch(() => undefined);
  const run = await started("turn-1");
  await runtime.turns.control("turn-1", "discord-private", "stop");
  const closed = runtime.close();
  // The stop asked before closing is still carried out, and waits for T3.
  await fake.waitFor(() => fake.interrupts.length === 1);
  expect(runtime.restartReadiness()).toEqual({
    state: "unready",
    reason: "a person's stop waits for T3 to confirm it",
  });
  await expect(runtime.turns.control("turn-1", "discord-private", "stop")).rejects.toThrow(
    "T3 runtime is closed",
  );
  fake.stop(threadId, run.id, "interrupted");
  await closed;
  expect(await core.outcome("turn-1")).toEqual({ outcome: "stopped" });
  await expect(reply).rejects.toThrow("the turn was stopped");
});

it("keeps a stop core could not record, and settles it first at the next start, never running it", async () => {
  const { first, long, waiting } = await oneWaiting();
  core.answer(503, 503, 503);
  await runtime.turns.control("turn-2", "discord-private", "stop");
  await fake.waitFor(() => settles("turn-2").length > 0);
  const before = core.received.length;
  await restart();
  expect(await core.outcome("turn-2")).toEqual({ outcome: "stopped" });
  await expect(waiting).rejects.toThrow("the turn was stopped");
  expect(calls(before).indexOf("GET /v1/runtime/turns")).toBeGreaterThan(
    calls(before).lastIndexOf("POST /v1/runtime/turns/turn-2/outcome"),
  );
  // The turn before it is taken up again; the stopped one never reaches T3.
  answer(long, "Long done.");
  expect((await first).output).toBe("Long done.");
  expect(runOf("turn-2")).toBeUndefined();
  expect(dispatches()).toHaveLength(1);
});

it("runs nothing for a late hand-over of a turn it settled", async () => {
  const { first, long, waiting } = await oneWaiting();
  await runtime.turns.control("turn-2", "discord-private", "stop");
  await expect(waiting).rejects.toThrow("the turn was stopped");
  await expect.poll(() => stopStatus("turn-2")).toBe(404);
  // A hand-over core sent before the stop arrives now.
  await runtime.turns.submit(turn("turn-2", "next"));
  answer(long, "Long done.");
  expect((await first).output).toBe("Long done.");
  // The conversation's next message runs next: the stopped one never does.
  const next = execute(turn("turn-3", "after"));
  answer(await started("turn-3"), "After.");
  expect((await next).output).toBe("After.");
  expect(runOf("turn-2")).toBeUndefined();
  expect(dispatches()).toHaveLength(2);
});

it("answers a message whose work goes on once its report's turn is open, asking core again meanwhile", async () => {
  const { first, parent } = await delegating();
  // As while the runtime is disabled, core opens no turn for it.
  core.answer(403, 401, 503);
  answer(parent, "DELEGATED");
  expect((await first).output).toBe("DELEGATED");
  expect(calls()).toEqual([
    "GET /v1/runtime/turns",
    ...Array<string>(4).fill("POST /v1/runtime/turns"),
    "POST /v1/runtime/turns/turn-1/outcome",
  ]);
  await fake.waitFor(() => runtime.activity("opened-1", "discord-private") !== undefined);
});

it("opens the report's turn at the next start when the runtime closed before core opened it", async () => {
  const { first, parent } = await delegating();
  core.answer(503, 503, 503);
  answer(parent, "DELEGATED");
  await fake.waitFor(() => openings().length > 0);
  await restart();
  expect((await first).output).toBe("DELEGATED");
  await fake.waitFor(() => runtime.activity("opened-1", "discord-private") !== undefined);
  expect(dispatches()).toHaveLength(1);
});

it("says when core will not open the report's turn, leaving the work to T3", async () => {
  const { first, parent } = await delegating();
  core.answer(409);
  answer(parent, "DELEGATED");
  expect((await first).output).toBe(
    "DELEGATED\n\nThe work this message started goes on in T3, but Carillon could not schedule its report here; T3 shows its result.",
  );
  expect(openings()).toHaveLength(1);
  expect(fake.interrupts).toEqual([]);
});

it("offers no control of a report's turn: the continuation is T3's", async () => {
  const { first, parent, child } = await delegating();
  answer(parent, "Working on it.");
  await first;
  const continuation = fake.finishDelegation(threadId, child.id);
  await fake.waitFor(() => runtime.activity("opened-1", "discord-private")?.phase === "model");
  expect(runtime.activity("opened-1", "discord-private")?.controls).toBeUndefined();
  const sent = fake.received.length;
  expect(await stopStatus("opened-1")).toBe(409);
  expect(fake.received.slice(sent)).toEqual([]);
  fake.assistant(threadId, continuation.id, "Child done.");
  fake.complete(threadId, continuation.id);
  expect((await settled("opened-1")).output).toBe("Child done.");
  expect(fake.interrupts).toEqual([]);
});

it("settles a turn core lists that it cannot read as failed, and takes up the others", async () => {
  core.hold({ ...turn("turn-1", "unreadable"), attachment_root: "" });
  core.hold(turn("turn-2", "hello"));
  await restart();
  expect(await core.outcome("turn-1")).toEqual({
    outcome: "failed",
    explanation: "Carillon could not read this turn back after its runtime restarted.",
  });
  answer(await started("turn-2"), "hi");
  expect((await settled("turn-2")).output).toBe("hi");
});

it("refuses a hand-over while it takes up the turns it held, before core's inbox gives up", async () => {
  const listing = core.stall("/v1/runtime/turns");
  await restart();
  const began = performance.now();
  await expect(runtime.turns.submit(turn("turn-1", "hello"))).rejects.toMatchObject({
    status: 503,
  });
  expect(performance.now() - began).toBeLessThan(10_000);
  listing.release();
  const reply = execute(turn("turn-1", "hello"));
  answer(await started("turn-1"), "hi");
  expect((await reply).output).toBe("hi");
}, 10_000);

it("keeps asking for the turns it held while core lists them in a form it cannot read, saying so once", async () => {
  core.hold(turn("turn-1", "hello"));
  core.answer(200, 200, 200);
  await restart();
  answer(await started("turn-1"), "hi");
  expect((await settled("turn-1")).output).toBe("hi");
  const told = vi
    .mocked(console.error)
    .mock.calls.filter(([line]) => String(line).includes("in a form this runtime cannot read"));
  expect(told).toHaveLength(1);
});

it("offers no stop of a message whose run answered while its report's turn opens", async () => {
  const { first, parent } = await delegating();
  const opening = core.stall("/v1/runtime/turns");
  answer(parent, "DELEGATED");
  await opening.reached;
  expect(runtime.activity("turn-1", "discord-private")).toBeUndefined();
  expect(await stopStatus("turn-1")).toBe(409);
  expect(runtime.restartReadiness()).toEqual({ state: "ready" });
  opening.release();
  expect((await first).output).toBe("DELEGATED");
  expect(fake.interrupts).toEqual([]);
});

it("is not ready to restart while a stop is kept neither by core nor in the stops file, writing it again until it is", async () => {
  const { first, long, waiting } = await oneWaiting();
  // A directory where the stops file goes refuses its write.
  await mkdir(stopsFile());
  core.answer(...Array<number>(6).fill(503));
  expect(await stopStatus("turn-2")).toBe(204);
  await fake.waitFor(() => settles("turn-2").length > 0);
  expect(runtime.restartReadiness()).toMatchObject({ state: "unready" });
  await rm(stopsFile(), { recursive: true });
  await expect.poll(() => runtime.restartReadiness()).toEqual({ state: "ready" });
  expect(JSON.parse(await readFile(stopsFile(), "utf8"))).toEqual(["turn-2"]);
  await restart();
  expect(await core.outcome("turn-2")).toEqual({ outcome: "stopped" });
  await expect(waiting).rejects.toThrow("the turn was stopped");
  answer(long, "Long done.");
  expect((await first).output).toBe("Long done.");
  expect(runOf("turn-2")).toBeUndefined();
});

it("refuses to start on a stops file it cannot read, naming it, and starts without one", async () => {
  expect(await readStops(stopsFile())).toEqual({ path: stopsFile(), kept: [] });
  await writeFile(stopsFile(), '["turn-1"]\n');
  expect(await readStops(stopsFile())).toEqual({ path: stopsFile(), kept: ["turn-1"] });
  for (const kept of ['["turn-1"', '{"turn-1":"stopped"}\n']) {
    await writeFile(stopsFile(), kept);
    await expect(readStops(stopsFile())).rejects.toThrow(`${stopsFile()}: cannot read`);
  }
  // Reading a directory fails as an unreadable file does, whoever runs the test.
  await rm(stopsFile());
  await mkdir(stopsFile());
  await expect(readStops(stopsFile())).rejects.toThrow("or remove it to start without them");
});
