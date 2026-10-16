/**
 * The runtime under test against a fake T3, fresh for each test of a file
 * that imports this: agent `default` is the assistant, on the profile
 * `claudeAgent`, and `code` is plain T3 Code.
 */
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { RuntimeOperations } from "@carillon/runtime/operations";
import { ModelSelections } from "@carillon/runtime/selections";
import { afterEach, beforeEach, vi } from "vitest";
import { derivedId, type Run } from "../contracts.ts";
import { startFakeT3, type FakeT3 } from "../fake.ts";
import { createT3Runtime, type T3Agent } from "../runtime.ts";
import type { Connect } from "../t3.ts";

export const PROJECT = "project-1";
export const selection = {
  instanceId: "claudeAgent",
  model: "claude-sonnet-5",
  options: [{ id: "effort", value: "low" }],
};
export let fake: FakeT3;
export let resources: string;
let selections: ModelSelections;
export let runtime: RuntimeOperations;

beforeEach(async () => {
  fake = await startFakeT3();
  resources = await mkdtemp(join(tmpdir(), "carillon-t3-"));
  await writeFile(join(resources, "SYSTEM.md"), "You are the Carillon agent.\n");
  selections = await ModelSelections.open(join(resources, "model-selection.json"));
  vi.spyOn(console, "log").mockImplementation(() => undefined);
  vi.spyOn(console, "error").mockImplementation(() => undefined);
  runtime = build();
});

afterEach(async () => {
  await runtime.close();
  await fake.close();
  await rm(resources, { recursive: true, force: true });
  vi.restoreAllMocks();
});

interface Overrides {
  readonly url?: string;
  readonly token?: string;
  readonly reconnectDelayMs?: number;
  readonly connect?: Connect;
  /** The default agent's role and settings model, the assistant's on the profile by default. */
  readonly role?: Pick<T3Agent, "assistant" | "selection">;
}

export function build(overrides: Overrides = {}) {
  return createT3Runtime({
    endpoint: { url: overrides.url ?? fake.url, token: overrides.token ?? fake.token },
    agents: new Map([
      [
        "default",
        {
          projectId: PROJECT,
          workspace: resources,
          ...(overrides.role ?? { assistant: true, selection }),
        },
      ],
      [
        "code",
        {
          projectId: PROJECT,
          workspace: resources,
          assistant: false,
          selection: { instanceId: "codex", model: "gpt-6-astra", options: [] },
        },
      ],
    ]),
    profiles: new Set(["claudeAgent"]),
    selections,
    ...(overrides.connect === undefined ? {} : { connect: overrides.connect }),
    reconnect: { attempts: 5, delayMs: overrides.reconnectDelayMs ?? 50 },
    settleMs: 500,
    timeoutMs: 2_000,
  });
}

/** A runtime started again over the same T3 and model ledger, as after a restart. */
export async function restart(overrides: Overrides = {}): Promise<void> {
  await runtime.close();
  selections = await ModelSelections.open(join(resources, "model-selection.json"));
  runtime = build(overrides);
}

export const threadId = derivedId(
  "thread",
  PROJECT,
  "discord-private",
  "default",
  "conversation-1",
);

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
