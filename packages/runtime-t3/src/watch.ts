/**
 * What one Carillon turn looks like in its T3 thread, folded from the thread
 * subscription: the run the turn's message started, whether that run still
 * needs anything, the answer it produced, and a bounded activity snapshot.
 * Nothing here talks to the network; a snapshot replaces everything seen
 * before, so a resumed subscription starts clean.
 */
import type { FinishedTool, RunningTool, TurnActivity, ActivityPhase } from "@carillon/protocol";
import { commandHeader, commentary, describe, displayed } from "@carillon/runtime-pi/activity";
import { bounded } from "@carillon/runtime-pi/failure";
import { isAbsolute, relative, resolve } from "node:path";
import type { Static, TSchema } from "typebox";
import { Check } from "typebox/value";
import {
  MessageSchema,
  ProjectionSchema,
  ProviderThreadSchema,
  RunSchema,
  RuntimeRequestSchema,
  TurnItemSchema,
  type Message,
  type ProviderThread,
  type Run,
  type RunStatus,
  type RuntimeRequest,
  type TurnItem,
} from "./contracts.ts";
import { T3Failure } from "./t3.ts";

export type Outcome =
  | { readonly kind: "completed"; readonly output: string }
  | { readonly kind: "failed"; readonly explanation: string }
  /** Someone other than this runtime ended the run. */
  | { readonly kind: "stopped"; readonly status: RunStatus }
  /** T3 waits for an approval or answer this runtime cannot give. */
  | { readonly kind: "blocked"; readonly explanation: string };

export interface TurnWatch {
  /** Folds one `subscribeThread` item in. */
  apply(item: unknown): void;
  /** Whether a snapshot has been seen since the last (re)subscription. */
  synchronized(): boolean;
  /** The run this turn's message started, once T3 recorded it. */
  run(): Run | undefined;
  /** The turn's run that is still active, or the follow-up T3 started after it. */
  activeRun(): Run | undefined;
  /** Earlier turns' runs that still occupy the thread. */
  blocking(): readonly Run[];
  /** Whether T3 listed background work for this turn at any point; it may wake the agent when that work ends. */
  sawBackgroundWork(): boolean;
  /** Whether the thread has any message yet. */
  empty(): boolean;
  outcome(): Outcome | undefined;
  activity(): TurnActivity;
  /** Called after every applied item, until the returned function is called. */
  onChange(listener: () => void): () => void;
}

const ACTIVE: ReadonlySet<RunStatus> = new Set([
  "preparing",
  "queued",
  "starting",
  "running",
  "waiting",
]);
const BEFORE_PROVIDER: ReadonlySet<RunStatus> = new Set(["preparing", "queued", "starting"]);
const TOOL_ITEMS: ReadonlySet<string> = new Set([
  "command_execution",
  "file_change",
  "file_search",
  "web_search",
  "dynamic_tool",
  "subagent",
]);
const RUNNING_ITEM: ReadonlySet<string> = new Set(["pending", "running", "waiting"]);
const FINISHED_ITEM: ReadonlySet<string> = new Set([
  "completed",
  "failed",
  "cancelled",
  "interrupted",
]);
const RECENT_CALLS = 5;
const RUNNING_CALLS = 8;

export function isActive(run: Run): boolean {
  return ACTIVE.has(run.status);
}

export function watchTurn(messageId: string, workspace: string): TurnWatch {
  let seen = false;
  let sawBackground = false;
  let runs = new Map<string, Run>();
  let requests = new Map<string, RuntimeRequest>();
  let providerThreads = new Map<string, ProviderThread>();
  let messages = new Map<string, Message>();
  let items = new Map<string, TurnItem>();
  const listeners = new Set<() => void>();
  /** When each background task was first seen, since T3 does not date them. */
  const backgroundSince = new Map<string, number>();

  function own(): Run | undefined {
    for (const run of runs.values()) if (run.userMessageId === messageId) return run;
    return undefined;
  }

  /**
   * The turn's run and every run T3 started after it, such as the wake that
   * delivers finished background work: Carillon dispatches nothing to the
   * thread until the turn settles, so later runs are part of it.
   */
  function turnRuns(): Run[] {
    const mine = own();
    if (mine === undefined) return [];
    return [...runs.values()]
      .filter((run) => run.ordinal >= mine.ordinal)
      .sort((left, right) => left.ordinal - right.ordinal);
  }

  function turnRunIds(): Set<string> {
    return new Set(turnRuns().map((run) => run.id));
  }

  function ownItems(): TurnItem[] {
    const ids = turnRunIds();
    return [...items.values()]
      .filter((item) => item.runId !== null && ids.has(item.runId))
      .sort((left, right) => left.ordinal - right.ordinal);
  }

  function active(): Run | undefined {
    return turnRuns().find(isActive);
  }

  function backgroundTasks(run: Run) {
    return (
      (run.providerThreadId === null
        ? undefined
        : providerThreads.get(run.providerThreadId)?.pendingBackgroundTasks) ?? []
    );
  }

  function answer(): string {
    const ids = turnRunIds();
    return [...messages.values()]
      .filter(
        (message) =>
          message.role === "assistant" && message.runId !== null && ids.has(message.runId),
      )
      .sort((left, right) => left.createdAt.localeCompare(right.createdAt))
      .map((message) => message.text.trim())
      .filter((text) => text.length > 0)
      .join("\n\n");
  }

  /** A pending request of this turn: one its items name, or any while the run waits. */
  function pendingRequest(run: Run): RuntimeRequest | undefined {
    const pending = [...requests.values()].filter((request) => request.status === "pending");
    const named = ownItems().flatMap((item) =>
      pending.filter((request) => request.id === item.requestId),
    );
    return named[0] ?? (run.status === "waiting" ? pending[0] : undefined);
  }

  /** What T3 is waiting for, in the person's terms, from the request's item. */
  function blockedBy(request: RuntimeRequest): string {
    const item = ownItems().find((candidate) => candidate.requestId === request.id);
    const question = item?.questions?.[0];
    if (item?.type === "user_input_request" && question !== undefined)
      return `T3 asked a question this runtime cannot relay yet (${question.header}: ${question.question}); the run was stopped. Send the answer in a new message.`;
    const kind = item?.requestKind ?? request.kind;
    return `T3 needs an approval (${kind}) this runtime cannot give yet; the run was stopped.`;
  }

  function tool(item: TurnItem): { tool: string; detail?: string } | undefined {
    switch (item.type) {
      case "command_execution":
        return { tool: "bash", ...withDetail(commandHeader(item.input)) };
      case "file_change":
        return {
          tool: "edit",
          ...withDetail(
            item.fileName === undefined ? undefined : displayed(workspacePath(item.fileName)),
          ),
        };
      case "file_search":
        return {
          tool: "grep",
          ...withDetail(item.pattern === undefined ? undefined : displayed(`"${item.pattern}"`)),
        };
      case "web_search":
        return {
          tool: "web_search",
          ...withDetail(
            item.patterns === undefined ? undefined : displayed(item.patterns.join(" ")),
          ),
        };
      case "dynamic_tool": {
        const name = item.toolName ?? "tool";
        const native = nativeTool(name, item.input);
        return {
          tool: bounded(name, 64) || "tool",
          ...withDetail(describe(native.tool, native.args, workspace)),
        };
      }
      case "subagent":
        return {
          tool: "subagent",
          ...withDetail(item.title === null ? undefined : displayed(item.title)),
        };
      default:
        return undefined;
    }
  }

  function workspacePath(path: string): string {
    const inside = relative(workspace, resolve(workspace, path));
    return inside.length === 0 || inside.startsWith("..") || isAbsolute(inside) ? path : inside;
  }

  function seconds(stamp: string | null): number {
    const parsed = Date.parse(stamp ?? "");
    return Number.isNaN(parsed) ? Math.floor(Date.now() / 1000) : Math.floor(parsed / 1000);
  }

  function replace<T extends { id: string }>(values: readonly T[]): Map<string, T> {
    return new Map(values.map((value) => [value.id, value]));
  }

  function parsed<S extends TSchema>(schema: S, payload: unknown, what: string): Static<S> {
    if (!Check(schema, payload))
      throw new T3Failure("malformed", `T3 sent a ${what} this runtime does not understand`);
    return payload;
  }

  return {
    apply(item) {
      if (!isRecord(item) || typeof item["kind"] !== "string")
        throw new T3Failure("malformed", "T3 sent a thread stream item without a kind");
      if (item["kind"] === "snapshot") {
        const projection = item["projection"];
        if (!Check(ProjectionSchema, projection))
          throw new T3Failure(
            "malformed",
            "T3 sent a thread snapshot this runtime does not understand",
          );
        runs = replace(projection.runs);
        requests = replace(projection.runtimeRequests);
        providerThreads = replace(projection.providerThreads);
        messages = replace(projection.messages);
        items = replace(projection.turnItems);
        seen = true;
      } else if (item["kind"] === "event") {
        const event = item["event"];
        if (!isRecord(event) || typeof event["type"] !== "string")
          throw new T3Failure("malformed", "T3 sent a thread event without a type");
        const payload = event["payload"];
        switch (event["type"]) {
          case "run.created":
          case "run.updated": {
            const run = parsed(RunSchema, payload, "run");
            runs.set(run.id, run);
            break;
          }
          case "runtime-request.updated": {
            const request = parsed(RuntimeRequestSchema, payload, "runtime request");
            requests.set(request.id, request);
            break;
          }
          case "provider-thread.updated": {
            const thread = parsed(ProviderThreadSchema, payload, "provider thread");
            providerThreads.set(thread.id, thread);
            break;
          }
          case "message.updated": {
            const message = parsed(MessageSchema, payload, "message");
            messages.set(message.id, message);
            break;
          }
          case "turn-item.updated": {
            const item = parsed(TurnItemSchema, payload, "turn item");
            items.set(item.id, item);
            break;
          }
          default:
            return;
        }
      } else return;
      const mine = own();
      if (mine !== undefined && backgroundTasks(mine).length > 0) sawBackground = true;
      for (const listener of listeners) listener();
    },
    synchronized: () => seen,
    run: own,
    activeRun: active,
    blocking() {
      const mine = own();
      return [...runs.values()].filter(
        (run) => isActive(run) && (mine === undefined || run.ordinal < mine.ordinal),
      );
    },
    sawBackgroundWork: () => sawBackground,
    empty: () => messages.size === 0,
    outcome() {
      const current = active();
      if (current !== undefined) {
        const request = pendingRequest(current);
        return request === undefined
          ? undefined
          : { kind: "blocked", explanation: blockedBy(request) };
      }
      const last = turnRuns().at(-1);
      if (last === undefined) return undefined;
      switch (last.status) {
        case "completed": {
          if (backgroundTasks(last).length > 0) return undefined;
          return { kind: "completed", output: answer() };
        }
        case "failed": {
          const error = ownItems().findLast(
            (item) => item.type === "error" && item.runId === last.id,
          );
          return { kind: "failed", explanation: error?.failure?.message ?? "The T3 run failed" };
        }
        case "interrupted":
        case "cancelled":
        case "rolled_back":
          return { kind: "stopped", status: last.status };
        case "preparing":
        case "queued":
        case "starting":
        case "running":
        case "waiting":
          return undefined;
      }
    },
    activity() {
      const run = active() ?? turnRuns().at(-1);
      const owned = ownItems();
      const toolItems = owned.filter((item) => TOOL_ITEMS.has(item.type));
      const running: RunningTool[] = toolItems
        .filter((item) => RUNNING_ITEM.has(item.status))
        .flatMap((item) => {
          const described = tool(item);
          return described === undefined
            ? []
            : [{ ...described, started_at: seconds(item.startedAt ?? item.updatedAt) }];
        });
      if (run !== undefined) {
        const now = Math.floor(Date.now() / 1000);
        const current = backgroundTasks(run);
        for (const task of current) {
          if (!backgroundSince.has(task.taskId)) backgroundSince.set(task.taskId, now);
          running.push({
            tool: task.kind ?? "background_task",
            ...withDetail(task.description === undefined ? undefined : displayed(task.description)),
            started_at: backgroundSince.get(task.taskId) ?? now,
          });
        }
        for (const id of backgroundSince.keys())
          if (!current.some((task) => task.taskId === id)) backgroundSince.delete(id);
      }
      const recent: FinishedTool[] = toolItems
        .filter((item) => FINISHED_ITEM.has(item.status))
        .sort((left, right) => (left.completedAt ?? "").localeCompare(right.completedAt ?? ""))
        .slice(-RECENT_CALLS)
        .flatMap((item) => {
          const described = tool(item);
          return described === undefined ? [] : [{ ...described, ok: item.status === "completed" }];
        });
      const ids = turnRunIds();
      const latest = [...messages.values()]
        .filter(
          (message) =>
            message.role === "assistant" && message.runId !== null && ids.has(message.runId),
        )
        .sort((left, right) => left.createdAt.localeCompare(right.createdAt))
        .at(-1);
      const headline = latest === undefined ? undefined : commentary(latest.text);
      return {
        phase: phase(run, owned, running.length),
        ...(headline === undefined ? {} : { headline }),
        running: running.slice(0, RUNNING_CALLS),
        recent,
        tool_calls: toolItems.length,
      };
    },
    onChange(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

function phase(
  run: Run | undefined,
  items: readonly TurnItem[],
  runningTools: number,
): ActivityPhase {
  if (run === undefined || BEFORE_PROVIDER.has(run.status)) return "starting";
  if (items.some((item) => item.type === "compaction" && RUNNING_ITEM.has(item.status)))
    return "compacting";
  if (
    items.some(
      (item) => item.type === "error" && item.retry !== undefined && RUNNING_ITEM.has(item.status),
    )
  )
    return "retrying";
  return runningTools > 0 ? "tools" : "model";
}

/** Claude's tool names and argument names in the vocabulary the shared description knows. */
function nativeTool(name: string, input: unknown): { tool: string; args: unknown } {
  const lower = name.toLowerCase();
  const tool = lower === "glob" ? "find" : lower;
  if (!isRecord(input) || typeof input["file_path"] !== "string" || "path" in input)
    return { tool, args: input };
  return { tool, args: { ...input, path: input["file_path"] } };
}

function withDetail(detail: string | undefined): { detail?: string } {
  return detail === undefined ? {} : { detail };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
