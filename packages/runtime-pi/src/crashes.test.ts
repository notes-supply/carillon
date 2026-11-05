import { spawn, type ChildProcess } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, it } from "vitest";
import { DEFAULT_RESTART_LIMIT } from "@earendil-works/pi-durable";
import { startFakeCore, type FakeCore } from "@carillon/runtime/testing";

const cleanups: (() => Promise<unknown>)[] = [];
afterEach(async () => {
  for (const cleanup of cleanups.splice(0).toReversed()) await cleanup();
});

async function until(holds: () => boolean, what: string): Promise<void> {
  for (let attempt = 0; attempt < 600; attempt += 1) {
    if (holds()) return;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error(`timed out waiting until ${what}`);
}

async function fakeCore(): Promise<FakeCore> {
  const core = await startFakeCore();
  cleanups.push(() => core.close());
  return core;
}

/**
 * The runtime of `crash-worker.ts` in a process of its own, handed
 * `message` as turn `turn-1`; `crash()` kills it with SIGKILL and starts it
 * again over the same storage.
 */
async function crashing(core: FakeCore, message: string) {
  const directory = await mkdtemp(join(tmpdir(), "carillon-runtime-pi-crash-"));
  cleanups.push(() => rm(directory, { recursive: true, force: true }));
  let child: ChildProcess | undefined;
  let printed = "";
  cleanups.push(() => kill());
  async function kill() {
    if (child === undefined || child.exitCode !== null || child.signalCode !== null) return;
    child.kill("SIGKILL");
    await once(child, "close");
  }
  async function start(...args: string[]) {
    const started = spawn(
      process.execPath,
      [join(import.meta.dirname, "crash-worker.ts"), directory, core.url, ...args],
      { stdio: ["ignore", "pipe", "pipe"] },
    );
    child = started;
    let stdout = "";
    let stderr = "";
    started.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString();
      printed += chunk.toString();
    });
    started.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString()));
    await until(
      () => stdout.includes("ready\n") || started.exitCode !== null,
      "the runtime is ready",
    );
    if (started.exitCode !== null) throw new Error(`the runtime exited: ${stderr}`);
  }
  await start(message);
  return {
    held: () => printed.split("hold\n").length - 1,
    async crash() {
      await kill();
      await start();
    },
  };
}

it("records an answer though crashes keep cutting off its posting", async () => {
  const core = await fakeCore();
  const outcome = "/v1/runtime/turns/turn-1/outcome";
  const posting = core.delay(outcome);
  const runtime = await crashing(core, "hello");
  for (let cutOff = 1; cutOff <= DEFAULT_RESTART_LIMIT; cutOff += 1) {
    await until(() => posting.arrived() >= cutOff, `the runtime posts ${String(cutOff)} times`);
    await runtime.crash();
  }
  await until(() => posting.arrived() > DEFAULT_RESTART_LIMIT, "the runtime posts again");
  posting.release();
  expect(await core.outcome("turn-1")).toMatchObject({
    outcome: "answered",
    output: "Answer: hello",
  });
  expect(core.received.filter((call) => call.path === outcome)).toHaveLength(1);
}, 60_000);

it("opens a scheduled turn though crashes keep cutting off its opening", async () => {
  const core = await fakeCore();
  const opening = core.delay("/v1/runtime/turns");
  const runtime = await crashing(core, "schedule");
  for (let cutOff = 1; cutOff <= DEFAULT_RESTART_LIMIT; cutOff += 1) {
    await until(() => opening.arrived() >= cutOff, `the runtime asks core ${String(cutOff)} times`);
    await runtime.crash();
  }
  await until(() => opening.arrived() > DEFAULT_RESTART_LIMIT, "the runtime asks core again");
  opening.release();
  const scheduled = await core.outcome("opened-1");
  expect(scheduled.outcome === "answered" && scheduled.output).toMatch(/ scheduled$/u);
}, 60_000);

it("reports a subagent's answer though crashes keep cutting off its reporter", async () => {
  const core = await fakeCore();
  const runtime = await crashing(core, "delegate");
  for (let cutOff = 1; cutOff <= DEFAULT_RESTART_LIMIT; cutOff += 1) {
    await until(() => runtime.held() >= cutOff, `the subagent holds ${String(cutOff)} times`);
    await runtime.crash();
  }
  await until(
    () => core.received.some((call) => call.path === "/v1/runtime/turns/opened-1/outcome"),
    "core records the report's outcome",
  );
  const report = await core.outcome("opened-1");
  expect(report.outcome === "answered" && report.output).toContain(
    `Held ${String(DEFAULT_RESTART_LIMIT)} times.`,
  );
}, 60_000);
