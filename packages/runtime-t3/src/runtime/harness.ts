/**
 * The runtime under test against a fake T3 and a fake core, fresh for each
 * test of a file that imports this: agents `default`, on Claude, and
 * `code`, on Codex.
 */
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { TurnActivity } from "@carillon/protocol";
import { createCoreClient } from "@carillon/runtime/core";
import { withReport } from "@carillon/runtime/failure";
import type { TurnRequest } from "@carillon/runtime/operations";
import { ModelSelections } from "@carillon/runtime/selections";
import { startFakeCore, type FakeCore } from "@carillon/runtime/testing";
import { afterEach, beforeEach, vi } from "vitest";
import { derivedId, type Run } from "../contracts.ts";
import { startFakeT3, type FakeT3 } from "../fake.ts";
import { readStops, type StopsFile } from "../held.ts";
import { createT3Runtime, type T3Agent } from "../runtime.ts";
import type { Connect } from "../t3.ts";

export const PROJECT = "project-1";
export const selection = {
  instanceId: "claudeAgent",
  model: "claude-sonnet-5",
  options: [{ id: "effort", value: "low" }],
};
export let fake: FakeT3;
export let core: FakeCore;
export let resources: string;
let selections: ModelSelections;
export let runtime: ReturnType<typeof createT3Runtime>;

beforeEach(async () => {
  fake = await startFakeT3();
  core = await startFakeCore();
  resources = await mkdtemp(join(tmpdir(), "carillon-t3-"));
  await writeFile(join(resources, "SYSTEM.md"), "You are the Carillon agent.\n");
  selections = await ModelSelections.open(join(resources, "model-selection.json"));
  vi.spyOn(console, "log").mockImplementation(() => undefined);
  vi.spyOn(console, "error").mockImplementation(() => undefined);
  runtime = await readBack(build);
});

afterEach(async () => {
  await runtime.close();
  await fake.close();
  await core.close();
  await rm(resources, { recursive: true, force: true });
});

interface Overrides {
  readonly url?: string;
  readonly token?: string;
  readonly projectId?: string;
  readonly reconnectDelayMs?: number;
  readonly connect?: Connect;
  /** The default agent's settings model. */
  readonly selection?: T3Agent["selection"];
  /** The stops file as the runtime reads it at start; by default, none kept yet. */
  readonly stops?: StopsFile;
}

export function build(overrides: Overrides = {}) {
  return createT3Runtime({
    endpoint: { url: overrides.url ?? fake.url, token: overrides.token ?? fake.token },
    agents: new Map([
      [
        "default",
        {
          projectId: overrides.projectId ?? PROJECT,
          workspace: resources,
          selection: overrides.selection ?? selection,
        },
      ],
      [
        "code",
        {
          projectId: overrides.projectId ?? PROJECT,
          workspace: resources,
          selection: { instanceId: "codex", model: "gpt-6-astra", options: [] },
        },
      ],
    ]),
    selections,
    stops: overrides.stops ?? { path: stopsFile(), kept: [] },
    core: createCoreClient({ url: core.url, providerId: "runtime-t3", token: "runtime-token" }),
    coreRetryMs: 20,
    ...(overrides.connect === undefined ? {} : { connect: overrides.connect }),
    reconnect: { attempts: 5, delayMs: overrides.reconnectDelayMs ?? 50 },
    settleMs: 500,
    timeoutMs: 2_000,
  });
}

export const stopsFile = () => join(resources, "stops.json");

/** A runtime started again over the same T3, model ledger and stops file, as after a restart. */
export async function restart(overrides: Overrides = {}): Promise<void> {
  await runtime.close();
  selections = await ModelSelections.open(join(resources, "model-selection.json"));
  const stops = await readStops(stopsFile());
  runtime = await readBack(() => build({ stops, ...overrides }));
}

/**
 * A runtime `start` makes, once core has listed it the turns core holds:
 * one a test holds later is not this runtime's to take up.
 */
export async function readBack(start: () => ReturnType<typeof createT3Runtime>) {
  const reads = () => core.received.filter(({ method }) => method === "GET").length;
  const before = reads();
  const started = start();
  await fake.waitFor(() => reads() > before);
  return started;
}

export const threadId = derivedId("thread", "discord-private", "default", "conversation-1");

/** A turn the runtime settled as failed; its message is the explanation it showed, if any. */
export class TurnFailed extends Error {}

/**
 * Hands `request` to `on` as core does and resolves with the answer it
 * settles the turn with, as {@link settled} does; `signal` stops the turn.
 */
export async function execute(request: TurnRequest, signal?: AbortSignal, on = runtime) {
  core.hold(request);
  const outcome = core.outcome(request.correlation_id);
  await on.turns.submit(request);
  // A turn that ended first has nothing to stop.
  const stop = () => {
    void on.turns
      .control(request.correlation_id, request.interface_id, "stop")
      .catch(() => undefined);
  };
  if (signal?.aborted === true) stop();
  else signal?.addEventListener("abort", stop, { once: true });
  return settled(request.correlation_id, signal, outcome);
}

/**
 * Resolves with the answer the runtime settles `turnId` with through core,
 * such as the report of delegated work in a turn it opened. A failure
 * rejects with a {@link TurnFailed} that keeps the turn's report, and a
 * stopped turn with `signal`'s reason.
 */
export async function settled(
  turnId: string,
  signal?: AbortSignal,
  outcome = core.outcome(turnId),
): Promise<{ readonly output: string; readonly activity?: TurnActivity }> {
  const ended = await outcome;
  if (ended.outcome === "answered")
    return {
      output: ended.output,
      ...(ended.activity === undefined ? {} : { activity: ended.activity }),
    };
  if (ended.outcome === "failed") {
    const failure = new TurnFailed(ended.explanation ?? "");
    throw ended.activity === undefined ? failure : withReport(failure, ended.activity);
  }
  throw signal?.reason ?? new Error(`the turn was ${ended.outcome}`);
}

/** The turns the runtime asked core to open, for reports of delegated work. */
export const openings = () =>
  core.received.filter(({ method, path }) => method === "POST" && path === "/v1/runtime/turns");

export function turn(correlation: string, input: string, extra: Record<string, unknown> = {}) {
  return {
    correlation_id: correlation,
    interface_id: "discord-private",
    agent_id: "default",
    audience_id: "conversation-1",
    input,
    resource_root: resources,
    attachment_root: join(resources, "attachments"),
    ...extra,
  };
}

export function thread() {
  const found = fake.threads.get(threadId);
  if (found === undefined) throw new Error("the conversation's thread does not exist");
  return found;
}

export function runOf(correlation: string): Run | undefined {
  return fake.threads
    .get(threadId)
    ?.runs.find((run) => run.userMessageId === derivedId("message", correlation));
}

export async function started(correlation: string): Promise<Run> {
  await fake.waitFor(() => runOf(correlation) !== undefined);
  const run = runOf(correlation);
  if (run === undefined) throw new Error("run not started");
  return run;
}

export function answer(run: Run, text: string): void {
  fake.assistant(threadId, run.id, text);
  fake.complete(threadId, run.id);
}

export const conversation = {
  interface_id: "discord-private",
  agent_id: "default",
  audience_id: "conversation-1",
};

export const dispatches = () =>
  fake.commands.filter((command) => command.type === "message.dispatch");
