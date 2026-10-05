/**
 * What one Carillon turn looks like in its T3 thread, folded from the thread
 * subscription. The fold keeps only what the turn needs: its run and the
 * runs T3 started after it, earlier runs that still occupy the thread, the
 * tools running now and the last few finished, open requests, the turn's
 * assistant messages, the provider thread's roster and session, plus a few
 * recent provider records awaiting attribution. History outside the turn is
 * dropped as it arrives. A snapshot replaces the fold's view of the thread
 * but keeps the turn's identity, its request items and its background facts
 * (work listed, report owed), which a bounded snapshot cannot restate.
 * Nothing here talks to the network.
 */
import type {
  ActivityPhase,
  FinishedTool,
  RunningTool,
  TurnActivity,
  WorkItem,
} from "@carillon/protocol";
import {
  commandHeader,
  commentary,
  describe,
  displayed,
  summarizeWork,
  toolName,
} from "@carillon/runtime/activity";
import { bounded } from "@carillon/runtime/failure";
import { isAbsolute, relative, resolve } from "node:path";
import type { Static, TSchema } from "typebox";
import { Check } from "typebox/value";
import {
  ContextTransferSchema,
  MessageSchema,
  ProjectionSchema,
  ProviderSessionDetachedSchema,
  ProviderSessionSchema,
  ProviderThreadSchema,
  RunSchema,
  RuntimeRequestSchema,
  SubagentSchema,
  TurnItemSchema,
  TurnItemSchemas,
  type BackgroundTask,
  type ContextTransfer,
  type Message,
  type Projection,
  type ProviderSession,
  type ProviderThread,
  type Run,
  type RunStatus,
  type RuntimeRequest,
  type Subagent,
  type TurnItemOf,
} from "./contracts.ts";
import { T3Failure } from "./t3.ts";

/**
 * Where a turn's report of delegated work continues: the message the
 * conversation's turn began with, and the last run whose text was answered.
 * The runtime hands it to core as the follow-up turn's cursor.
 */
export interface FollowUp {
  readonly message: string;
  readonly after: number;
}

export type Outcome =
  /** `followUp`: work this answer started still owes the conversation a report. */
  | { readonly kind: "completed"; readonly output: string; readonly followUp?: FollowUp }
  | { readonly kind: "failed"; readonly explanation: string }
  /** T3's state cannot show whether the turn's background work was reported. */
  | { readonly kind: "unresolved"; readonly explanation: string }
  /** Someone other than this runtime ended the run. */
  | { readonly kind: "stopped"; readonly status: RunStatus }
  /** T3 waits for an approval or answer this runtime cannot give; `asked` says which. */
  | { readonly kind: "blocked"; readonly asked: string };

export interface Retained {
  readonly runs: number;
  readonly running: number;
  readonly recent: number;
  readonly counted: number;
  readonly requests: number;
  readonly messages: number;
  readonly providerThreads: number;
  readonly sessions: number;
  readonly pendingProviderRecords: number;
  readonly detachedSessions: number;
  readonly delegations: number;
  readonly transfers: number;
}

export interface TurnWatch {
  /** Folds one `subscribeThread` item in; kinds this build does not know are ignored. */
  apply(item: unknown): void;
  /** Forgets that the subscription caught up; the next snapshot or marker says so again. */
  resubscribing(): void;
  /** Whether the current subscription has delivered its snapshot or replay. */
  synchronized(): boolean;
  /** The last event sequence folded: the native anchor a resumed subscription continues from. */
  sequence(): number | undefined;
  /** The run this turn's message started, as last seen. */
  run(): Run | undefined;
  /** The turn's run or a follow-up T3 started after it, while one is active. */
  activeRun(): Run | undefined;
  /** The thread's latest run, the one T3 settles background work through. */
  latestRun(): Run | undefined;
  /** Whether runs this turn did not start still occupy the thread, so a new message waits behind them. */
  busy(): boolean;
  /** Whether the thread has any message yet. */
  empty(): boolean;
  outcome(): Outcome | undefined;
  /** Nothing of the turn still executes: no active run, no background work, and no report its live process still owes. */
  quiet(): boolean;
  /** The provider session behind the turn's latest run, while T3 lists one. */
  providerSession(): string | undefined;
  /** Whether a request of the turn is still open. */
  requestOpen(): boolean;
  activity(): TurnActivity;
  retained(): Retained;
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
const RUNNING_ITEM: ReadonlySet<string> = new Set(["pending", "running", "waiting"]);
const FINISHED_ITEM: ReadonlySet<string> = new Set([
  "completed",
  "failed",
  "cancelled",
  "interrupted",
]);
const ENDED_SESSION: ReadonlySet<ProviderSession["status"]> = new Set(["stopped", "error"]);
/** T3's own test for a child still working (`isOrchestrationV2WorkActive`). */
const ACTIVE_TASK: ReadonlySet<Subagent["status"]> = new Set(["pending", "running", "waiting"]);
/** A published result T3 has yet to deliver to the parent in a continuation. */
const OWED_DELIVERY: ReadonlySet<string> = new Set(["pending", "claimed"]);
/** Transfers shown as work of their own; spawn and result transfers belong to their delegation. */
const TRANSFER_WORK: ReadonlyMap<string, { kind: WorkItem["kind"]; label: string }> = new Map([
  ["provider_handoff", { kind: "handoff", label: "context handoff" }],
  ["fork", { kind: "fork", label: "fork to another thread" }],
  ["merge_back", { kind: "fork", label: "merge back from another thread" }],
]);
const RECENT_CALLS = 5;
const RUNNING_CALLS = 8;
/** Provider records kept while the fold cannot yet tell whether the turn owns them. */
const RECENT_RECORDS = 4;
const KNOWN_ITEMS: ReadonlySet<string> = new Set(Object.keys(TurnItemSchemas));

type ToolItem = TurnItemOf<
  "command_execution" | "file_change" | "file_search" | "web_search" | "dynamic_tool" | "subagent"
>;
type RequestItem = TurnItemOf<"user_input_request" | "approval_request">;

export function isActive(run: Run): boolean {
  return ACTIVE.has(run.status);
}

/**
 * Whether anything on the thread still runs or is owed, whoever started it:
 * an active run, background work a provider thread lists, or a child agent
 * still working or with a result T3 has yet to deliver.
 */
export function threadWorking(projection: Projection): boolean {
  return (
    projection.runs.some(isActive) ||
    projection.providerThreads.some((thread) => (thread.pendingBackgroundTasks ?? []).length > 0) ||
    projection.subagents.some(
      (task) =>
        ACTIVE_TASK.has(task.status) || OWED_DELIVERY.has(task.completionDelivery?.state ?? ""),
    )
  );
}

export interface WatchOptions {
  /**
   * An earlier turn on this thread ended with a stop T3 did not confirm.
   * Its process may still deliver a wake, and T3 labels every wake alike, so
   * no follow-up of this turn can be trusted as its own: the turn ends
   * unresolved as soon as one is adopted. The runtime owns this fact.
   */
  readonly uncertainPredecessor?: boolean;
  /**
   * This turn reports delegated work of an earlier turn that began with
   * `messageId`: it owns no run of its own, only the continuations T3
   * starts for that work after run ordinal `after`.
   */
  readonly followUp?: { readonly after: number };
}

export function watchTurn(
  messageId: string,
  workspace: string,
  options: WatchOptions = {},
): TurnWatch {
  let seen = false;
  let lastSequence: number | undefined;
  /** The turn's run as last seen: its identity, kept through snapshots and reconnections. */
  let anchor: Run | undefined;
  /** Runs from the turn's onward, by ID, with the greatest ordinal tracked. */
  let turnRuns = new Map<string, Run>();
  let last: Run | undefined;
  /** Earlier runs still active: a new message waits behind them in T3's queue. */
  let earlier = new Map<string, Run>();
  const followUp = options.followUp;
  /**
   * Continuations T3 started to report work the turn's runs delegated,
   * positively linked through T3's own records. A turn never answers with
   * them; a follow-up turn of the same conversation does (see `followUp`).
   */
  let continuationRuns = new Map<string, Run>();
  /** Continuation messages T3 announced or sent, by message, with the run whose work they report. */
  const continuations = new Map<string, string>();
  /** Later runs not (yet) known to be the turn's, kept briefly in case a continuation link arrives after them. */
  const unclaimed = new Map<string, Run>();
  /** Runs adopted because the turn's process owed a background report (wakes), not by a native link. */
  const wakeRuns = new Set<string>();
  /** Child agents started by the turn's runs or their continuations, kept across snapshots. */
  const delegations = new Map<string, Subagent>();
  /** Delegations a resumed snapshot no longer listed: their state is not confirmed. */
  const unconfirmed = new Set<string>();
  /** Handoffs, forks and merges of the turn's runs. */
  const transfers = new Map<string, ContextTransfer>();
  let hasMessages = false;
  /** The turn's assistant messages by ID, kept across snapshots. */
  const assistant = new Map<string, Message>();
  let latestAssistant: Message | undefined;
  let running = new Map<string, ToolItem>();
  let recent: ToolItem[] = [];
  let counted = new Set<string>();
  const requestItems = new Map<string, RequestItem>();
  let pendingRequests = new Map<string, RuntimeRequest>();
  let errors = new Map<string, string>();
  let compacting = new Set<string>();
  let retrying = new Set<string>();
  let providerThreads = new Map<string, ProviderThread>();
  let sessions = new Map<string, ProviderSession>();
  /**
   * The few most recent provider records not (yet) owned by a turn run. T3
   * sends a run's provider thread just before `run.created`, and its session
   * just before the thread record it names when the turn starts, so each is
   * seen before the fold can tell it belongs to the turn.
   */
  const recentThreads = new Map<string, ProviderThread>();
  const recentSessions = new Map<string, ProviderSession>();
  /** Sessions T3 detached, or omitted from a snapshot while a turn thread still names them. */
  const detachedSessions = new Set<string>();
  /**
   * The ordinal of a turn run seen completed while its roster still listed
   * work. Its process delivers the result in a run of its own, so the turn
   * is not complete until a later run exists.
   */
  let wakeOwedAfter: number | undefined;
  /**
   * Later runs first seen while this turn's run was still active. This
   * turn's own wake can only start after its run completed with work still
   * listed, so such a run is another process's, for good.
   */
  const foreign = new Set<string>();
  /** A completed run followed by a run no fact of this turn accounts for. */
  let stray: string | undefined;
  /**
   * The ordinal of a turn run whose roster listed work the last time work
   * was seen. Cleared when the roster empties while that run still executes
   * (the result reached the running turn). Left set across a resumed
   * subscription that missed the transition, when T3's state alone cannot
   * say whether the result was delivered.
   */
  let backgroundSeenAt: number | undefined;
  const listeners = new Set<() => void>();
  /** When each background task was first seen, since T3 does not date them. */
  const backgroundSince = new Map<string, number>();

  /**
   * Forgets everything but the turn's identity, its request items, its
   * background facts and the assistant messages of its own runs already
   * observed: a bounded snapshot may omit earlier messages, and a message it
   * does list replaces the observed one by ID.
   */
  function reset(): void {
    turnRuns = new Map();
    last = undefined;
    earlier = new Map();
    continuationRuns = new Map();
    unclaimed.clear();
    hasMessages = false;
    running = new Map();
    recent = [];
    counted = new Set();
    pendingRequests = new Map();
    errors = new Map();
    compacting = new Set();
    retrying = new Set();
    providerThreads = new Map();
    sessions = new Map();
    stray = undefined;
    recentThreads.clear();
    recentSessions.clear();
    detachedSessions.clear();
  }

  /**
   * Whether a run T3 started after this turn's is this turn's. This turn's
   * own wake can only start once its run completed with work still listed,
   * so a later run first seen while the run is active is another process's
   * (such as an earlier turn's late wake) and stays foreign. After that, a
   * later run is the owed wake, or resolves a resumed snapshot's ambiguity.
   * A later run no fact of this turn accounts for is a stray: neither
   * answered with nor dropped, it makes the turn unresolved.
   */
  function adopt(run: Run): boolean {
    if (anchor === undefined || run.ordinal <= anchor.ordinal) return false;
    if (turnRuns.has(run.id)) return true;
    // A continuation is identified by T3's own record, whenever it is seen:
    // it is neither another process's wake nor a stray, and only a
    // follow-up turn answers with it.
    if (continuationOf(run)) {
      continuationRuns.set(run.id, run);
      unclaimed.delete(run.userMessageId);
      if (stray === run.id) stray = undefined;
      return followUp !== undefined && run.ordinal > followUp.after;
    }
    remember(unclaimed, run.userMessageId, run);
    if (foreign.has(run.id)) return false;
    if (followUp !== undefined) {
      // A follow-up turn owns only its continuations and the wakes they owe;
      // anything else on the thread is someone else's and is left alone.
      if (wakeOwedAfter !== undefined && run.ordinal > wakeOwedAfter) return wake(run);
      return false;
    }
    if (anchor.status !== "completed") {
      foreign.add(run.id);
      return false;
    }
    if (wakeOwedAfter !== undefined && run.ordinal > wakeOwedAfter) return wake(run);
    if (backgroundSeenAt !== undefined) return wake(run);
    stray ??= run.id;
    return false;
  }

  function wake(run: Run): true {
    wakeRuns.add(run.id);
    return true;
  }

  /** Whether T3 linked this run's message to work of a run the turn holds. */
  function continuationOf(run: Run): boolean {
    const parent = continuations.get(run.userMessageId);
    return parent !== undefined && linkedRun(parent);
  }

  /** A run whose delegated work and continuations belong to this turn's report. */
  function linkedRun(id: string): boolean {
    return anchor?.id === id || turnRuns.has(id) || continuationRuns.has(id);
  }

  /** Records a continuation link, and folds again a run it arrived after. */
  function link(message: string, parentRun: string): void {
    if (continuations.get(message) === parentRun) return;
    continuations.set(message, parentRun);
    const run = unclaimed.get(message);
    if (run !== undefined) foldRun(run);
  }

  function foldRun(run: Run): void {
    if (run.userMessageId === messageId) anchor = run;
    const delivery = run.delegatedCompletion?.delivery;
    if (delivery != null) continuations.set(delivery.messageId, run.id);
    const owned =
      anchor !== undefined && ((run.id === anchor.id && followUp === undefined) || adopt(run));
    if (owned) {
      turnRuns.set(run.id, run);
      earlier.delete(run.id);
      if (last === undefined || run.ordinal >= last.ordinal) last = run;
      return;
    }
    if (anchor !== undefined && run.ordinal >= anchor.ordinal) return;
    if (isActive(run)) earlier.set(run.id, run);
    else earlier.delete(run.id);
  }

  /** An adopted wake whose provenance an unconfirmed earlier stop leaves uncertain. */
  function uncertainFollowUp(): boolean {
    if (options.uncertainPredecessor !== true || anchor === undefined) return false;
    for (const run of turnRuns.values()) if (wakeRuns.has(run.id)) return true;
    return false;
  }

  /**
   * Whether work the turn delegated still owes this conversation a report:
   * a T3-owned task still working, or with a result T3 has yet to deliver
   * in a continuation (T3 publishes the result before it schedules the
   * continuation, so this covers the gap between them), an announced
   * continuation not started yet, or a continuation no turn answered with.
   */
  function reportOwed(): boolean {
    for (const task of delegations.values())
      if (
        task.origin === "app_owned" &&
        (ACTIVE_TASK.has(task.status) || OWED_DELIVERY.has(task.completionDelivery?.state ?? ""))
      )
        return true;
    const started = new Set([...continuationRuns.values()].map((run) => run.userMessageId));
    for (const [message, parent] of continuations)
      if (linkedRun(parent) && !started.has(message)) return true;
    for (const run of continuationRuns.values())
      if (followUp === undefined ? true : run.ordinal > followUp.after && !turnRuns.has(run.id))
        return true;
    return false;
  }

  /** Runs seen before the turn's own were filed as earlier; refile those from the turn onward. */
  function refile(): void {
    if (anchor === undefined) return;
    for (const run of [...earlier.values()]) if (run.ordinal >= anchor.ordinal) foldRun(run);
  }

  function foldMessage(message: Message): void {
    hasMessages = true;
    if (message.delegatedCompletion !== undefined)
      link(message.id, message.delegatedCompletion.parentRunId);
    if (message.role !== "assistant" || message.runId === null || !turnRuns.has(message.runId))
      return;
    assistant.set(message.id, message);
    if (
      latestAssistant === undefined ||
      latestAssistant.id === message.id ||
      message.createdAt >= latestAssistant.createdAt
    )
      latestAssistant = message;
  }

  function foldItem(payload: unknown): void {
    if (!isRecord(payload)) return;
    const type = payload["type"];
    const runId = payload["runId"];
    // Only the turn's own items of the kinds this runtime reads are checked and kept.
    if (typeof type !== "string" || !KNOWN_ITEMS.has(type)) return;
    if (typeof runId !== "string" || !turnRuns.has(runId)) return;
    const item = parsed(TurnItemSchema, payload, `${type} item`);
    if (item.runId === null) return;
    switch (item.type) {
      case "command_execution":
      case "file_change":
      case "file_search":
      case "web_search":
      case "dynamic_tool":
      case "subagent":
        counted.add(item.id);
        if (RUNNING_ITEM.has(item.status)) running.set(item.id, item);
        else {
          running.delete(item.id);
          if (FINISHED_ITEM.has(item.status)) {
            recent = recent.filter((finished) => finished.id !== item.id);
            recent.push(item);
            if (recent.length > RECENT_CALLS) recent.shift();
          }
        }
        return;
      case "user_input_request":
      case "approval_request":
        requestItems.set(item.requestId, item);
        return;
      case "error":
        errors.set(item.runId, item.failure.message);
        if (item.retry !== undefined && RUNNING_ITEM.has(item.status)) retrying.add(item.id);
        else retrying.delete(item.id);
        return;
      case "compaction":
        if (RUNNING_ITEM.has(item.status)) compacting.add(item.id);
        else compacting.delete(item.id);
        return;
    }
  }

  /** A child agent of a run the turn holds; others are another conversation's business. */
  function foldSubagent(task: Subagent): void {
    if (task.runId === null || !linkedRun(task.runId)) return;
    delegations.set(task.id, task);
    unconfirmed.delete(task.id);
  }

  function foldTransfer(transfer: ContextTransfer): void {
    if (!TRANSFER_WORK.has(transfer.type)) return;
    const runs = [transfer.sourcePoint.runId, transfer.targetRunId];
    if (!runs.some((run) => run != null && linkedRun(run))) return;
    transfers.set(transfer.id, transfer);
  }

  function foldRequest(request: RuntimeRequest): void {
    if (request.status === "pending") pendingRequests.set(request.id, request);
    else pendingRequests.delete(request.id);
  }

  /**
   * The background work a run's provider thread lists. T3 sends the thread
   * record before the run that names it and lists provider threads whole in
   * a snapshot, so a run that names a thread this fold has not been given
   * is a broken reference: its work is unknown, never an empty roster.
   */
  function roster(run: Run | undefined): readonly BackgroundTask[] {
    if (run?.providerThreadId == null) return [];
    return listed(providerThreads.get(run.providerThreadId), run);
  }

  function listed(thread: ProviderThread | undefined, run: Run): readonly BackgroundTask[] {
    if (thread === undefined)
      throw new T3Failure(
        "malformed",
        `T3 named provider thread ${String(run.providerThreadId)} for run ${run.id} without sending its record`,
      );
    return thread.pendingBackgroundTasks ?? [];
  }

  /**
   * Provider records matter only for the turn's runs: the thread's history of
   * provider threads and sessions is dropped as it arrives, and before the
   * turn's run exists only the few most recent records wait in the
   * attribution window below. T3 sends a run's provider thread record just
   * before `run.created`, and its session record just before the thread
   * record that names it when the turn starts, so the turn's records arrive
   * with the run.
   */
  function turnProviderThread(id: string): boolean {
    for (const run of turnRuns.values()) if (run.providerThreadId === id) return true;
    return false;
  }

  function turnSession(id: string): boolean {
    for (const thread of providerThreads.values()) if (thread.providerSessionId === id) return true;
    return false;
  }

  function foldProviderThread(thread: ProviderThread): void {
    if (turnProviderThread(thread.id)) {
      providerThreads.set(thread.id, thread);
      pruneProviders();
    } else remember(recentThreads, thread.id, thread);
  }

  function foldSession(session: ProviderSession): void {
    if (turnSession(session.id)) {
      sessions.set(session.id, session);
      detachedSessions.delete(session.id);
    } else remember(recentSessions, session.id, session);
  }

  /** Keeps `record` among the few most recent of `recent`. */
  function remember<T>(recent: Map<string, T>, key: string, record: T): void {
    recent.delete(key);
    recent.set(key, record);
    for (const oldest of recent.keys()) {
      if (recent.size <= RECENT_RECORDS) break;
      recent.delete(oldest);
    }
  }

  /** Adopts recently seen records the turn's runs and threads turn out to refer to. */
  function adoptPending(): void {
    for (const run of turnRuns.values()) {
      const id = run.providerThreadId;
      if (id === null || providerThreads.has(id)) continue;
      const thread = recentThreads.get(id);
      if (thread === undefined) continue;
      recentThreads.delete(id);
      providerThreads.set(id, thread);
    }
    for (const thread of providerThreads.values()) {
      const id = thread.providerSessionId;
      if (id === null || sessions.has(id)) continue;
      const session = recentSessions.get(id);
      if (session === undefined) continue;
      recentSessions.delete(id);
      sessions.set(id, session);
    }
  }

  /** Drops provider records no retained run refers to any more, then adopts pending ones. */
  function pruneProviders(): void {
    for (const id of providerThreads.keys())
      if (!turnProviderThread(id)) providerThreads.delete(id);
    for (const id of sessions.keys()) if (!turnSession(id)) sessions.delete(id);
    for (const id of detachedSessions) if (!turnSession(id)) detachedSessions.delete(id);
    adoptPending();
  }

  /** The turn's process owes a report in a run that has not appeared, and T3 has not shown it stopped. */
  function wakeOwed(): boolean {
    return (
      last?.status === "completed" &&
      wakeOwedAfter !== undefined &&
      last.ordinal <= wakeOwedAfter &&
      !sessionStopped(last)
    );
  }

  /**
   * The turn's run completed, its roster is empty, and the subscription did
   * not witness how the work it once listed ended: delivered inside the run,
   * or still owed. T3's current state does not distinguish the two.
   */
  function ambiguous(): boolean {
    return (
      last?.status === "completed" &&
      roster(last).length === 0 &&
      !(wakeOwedAfter !== undefined && last.ordinal <= wakeOwedAfter) &&
      backgroundSeenAt !== undefined &&
      last.ordinal <= backgroundSeenAt
    );
  }

  function sessionOf(
    run: Run,
  ): { readonly id: string | null; readonly record: ProviderSession | undefined } | undefined {
    if (run.providerThreadId === null) return undefined;
    const thread = providerThreads.get(run.providerThreadId);
    if (thread === undefined) return undefined;
    const id = thread.providerSessionId;
    return { id, record: id === null ? undefined : sessions.get(id) };
  }

  /** T3 reports the run's provider process stopped or failed: it can neither report nor wake. */
  function sessionStopped(run: Run): boolean {
    const record = sessionOf(run)?.record;
    return record !== undefined && ENDED_SESSION.has(record.status);
  }

  /**
   * T3 detached the run's provider session (its thread names none, T3 sent
   * the detach, or a snapshot omitted the session the thread names). T3
   * removes the record before the process closes and does not fence what
   * that process may still deliver, so this is not a stop. A session record
   * merely not seen yet is neither detached nor stopped.
   */
  function sessionDetached(run: Run): boolean {
    const session = sessionOf(run);
    return session !== undefined && (session.id === null || detachedSessions.has(session.id));
  }

  function active(): Run | undefined {
    for (const run of turnRuns.values()) if (isActive(run)) return run;
    return undefined;
  }

  /**
   * A pending request of this turn: one a retained request item of the turn
   * names. A pending request no item of the turn names stays unassigned,
   * however the turn's run waits: it may be another run's leftover.
   */
  function ownedPendingRequest(): RuntimeRequest | undefined {
    for (const request of pendingRequests.values())
      if (requestItems.has(request.id)) return request;
    return undefined;
  }

  /** What T3 is waiting for, in the person's terms, from the request's item. */
  function asked(request: RuntimeRequest): string {
    const item = requestItems.get(request.id);
    const question = item?.type === "user_input_request" ? item.questions[0] : undefined;
    if (question !== undefined)
      return `T3 asked a question this runtime cannot relay yet (${question.header}: ${question.question})`;
    const kind = item?.type === "approval_request" ? item.requestKind : request.kind;
    return `T3 needs an approval (${kind}) this runtime cannot give yet`;
  }

  function answer(): string {
    return [...assistant.values()]
      .sort((left, right) => left.createdAt.localeCompare(right.createdAt))
      .map((message) => message.text.trim())
      .filter((text) => text.length > 0)
      .join("\n\n");
  }

  function tool(item: ToolItem): { tool: string; detail?: string } {
    switch (item.type) {
      case "command_execution":
        return { tool: "bash", ...withDetail(commandHeader(item.input)) };
      case "file_change":
        return { tool: "edit", ...withDetail(displayed(workspacePath(item.fileName))) };
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

  function parsed<S extends TSchema>(schema: S, payload: unknown, what: string): Static<S> {
    if (!Check(schema, payload))
      throw new T3Failure("malformed", `T3 sent a ${what} this runtime does not understand`);
    return payload;
  }

  /**
   * The turn's delegated and transferred work, from T3's own records: each
   * child agent with the state T3 last confirmed (unknown once a snapshot
   * stopped listing it), the report T3 still owes this conversation, and
   * the handoffs, forks and merges of the turn's runs.
   */
  function workItems(): WorkItem[] {
    const items: WorkItem[] = [];
    for (const task of delegations.values()) {
      const actor = task.model ?? task.driver;
      items.push({
        id: task.id,
        kind: "delegation",
        label: (task.title === null ? undefined : displayed(task.title, 100)) ?? "subagent",
        state: unconfirmed.has(task.id) ? "unknown" : TASK_STATE[task.status],
        ...(delegations.has(task.parentNodeId) ? { parent: task.parentNodeId } : {}),
        ...(actor.length === 0 ? {} : { actor: toolName(actor) }),
        ...(task.startedAt === null ? {} : { started_at: seconds(task.startedAt) }),
      });
    }
    const reporting = [...continuationRuns.values()].some(
      (run) => isActive(run) && !turnRuns.has(run.id),
    );
    if (reporting || (followUp === undefined && reportOwed()))
      items.push({
        id: `report:${messageId}`,
        kind: "task",
        label: "report back to this conversation",
        state: reporting ? "running" : "queued",
      });
    for (const transfer of transfers.values()) {
      const work = TRANSFER_WORK.get(transfer.type);
      if (work === undefined) continue;
      const target =
        transfer.type === "provider_handoff" ? transfer.targetProviderInstanceId : null;
      items.push({
        id: transfer.id,
        kind: work.kind,
        label: target === null ? work.label : `${work.label} to ${toolName(target)}`,
        state: transferState(transfer.status),
      });
    }
    return items;
  }

  function quiet(): boolean {
    return (
      active() === undefined &&
      roster(last).length === 0 &&
      !wakeOwed() &&
      !(ambiguous() && last !== undefined && !sessionStopped(last))
    );
  }

  return {
    apply(item) {
      if (!isRecord(item) || typeof item["kind"] !== "string")
        throw new T3Failure("malformed", "T3 sent a thread stream item without a kind");
      switch (item["kind"]) {
        case "snapshot": {
          const projection = item["projection"];
          const sequence = item["snapshotSequence"];
          if (!Check(ProjectionSchema, projection) || typeof sequence !== "number")
            throw new T3Failure(
              "malformed",
              "T3 sent a thread snapshot this runtime does not understand",
            );
          // A snapshot lists the thread's runs whole: one that omits a run
          // this turn already observed is broken, and is refused before it
          // replaces anything the fold knows.
          for (const id of turnRuns.keys())
            if (!projection.runs.some((run) => run.id === id))
              throw new T3Failure(
                "malformed",
                `T3 sent a thread snapshot that omits run ${id} this turn observed`,
              );
          reset();
          // Continuation links first, from messages and the runs that announce
          // them, so no continuation is ever judged a stray; then runs in
          // ordinal order, so the turn's own is known before the runs that
          // follow it are judged.
          for (const message of projection.messages)
            if (message.delegatedCompletion !== undefined)
              continuations.set(message.id, message.delegatedCompletion.parentRunId);
          for (const run of [...projection.runs].sort((a, b) => a.ordinal - b.ordinal))
            foldRun(run);
          refile();
          // Delegations and transfers arrive whole; one the turn knew that
          // this snapshot omits is no longer confirmed, never assumed done.
          const listedTasks = new Set(projection.subagents.map((task) => task.id));
          for (const id of delegations.keys()) if (!listedTasks.has(id)) unconfirmed.add(id);
          for (const task of projection.subagents) foldSubagent(task);
          for (const transfer of projection.contextTransfers) foldTransfer(transfer);
          for (const thread of projection.providerThreads) foldProviderThread(thread);
          for (const session of projection.providerSessions) foldSession(session);
          // A snapshot lists every session the thread still has: one a turn
          // thread names but the snapshot omits was detached.
          for (const thread of providerThreads.values())
            if (thread.providerSessionId !== null && !sessions.has(thread.providerSessionId))
              detachedSessions.add(thread.providerSessionId);
          for (const request of projection.runtimeRequests) foldRequest(request);
          for (const payload of projection.turnItems) foldItem(payload);
          for (const message of projection.messages) foldMessage(message);
          lastSequence = sequence;
          seen = true;
          break;
        }
        case "event": {
          const event = item["event"];
          const sequence = item["sequence"];
          if (!isRecord(event) || typeof event["type"] !== "string" || typeof sequence !== "number")
            throw new T3Failure(
              "malformed",
              "T3 sent a thread event this runtime does not understand",
            );
          // A resumed subscription may replay events already folded.
          if (lastSequence !== undefined && sequence <= lastSequence) return;
          lastSequence = sequence;
          const payload = event["payload"];
          switch (event["type"]) {
            case "run.created":
            case "run.updated":
              foldRun(parsed(RunSchema, payload, "run"));
              refile();
              pruneProviders();
              break;
            case "runtime-request.updated":
              foldRequest(parsed(RuntimeRequestSchema, payload, "runtime request"));
              break;
            case "provider-thread.updated":
              foldProviderThread(parsed(ProviderThreadSchema, payload, "provider thread"));
              break;
            case "provider-session.attached":
            case "provider-session.updated":
              foldSession(parsed(ProviderSessionSchema, payload, "provider session"));
              break;
            case "provider-session.detached": {
              const { providerSessionId: id } = parsed(
                ProviderSessionDetachedSchema,
                payload,
                "provider session detach",
              );
              if (turnSession(id)) {
                sessions.delete(id);
                recentSessions.delete(id);
                detachedSessions.add(id);
              }
              break;
            }
            case "message.updated":
              foldMessage(parsed(MessageSchema, payload, "message"));
              break;
            case "subagent.updated":
              foldSubagent(parsed(SubagentSchema, payload, "subagent"));
              break;
            case "context-transfer.created":
            case "context-transfer.updated":
              foldTransfer(parsed(ContextTransferSchema, payload, "context transfer"));
              break;
            case "turn-item.updated":
              foldItem(payload);
              break;
            default:
              return;
          }
          break;
        }
        case "synchronized":
          seen = true;
          break;
        default:
          return;
      }
      if (last !== undefined) {
        const listed = roster(last).length > 0;
        if (listed) backgroundSeenAt = last.ordinal;
        else if (isActive(last) && backgroundSeenAt === last.ordinal) backgroundSeenAt = undefined;
        if (listed && last.status === "completed") wakeOwedAfter = last.ordinal;
      }
      for (const listener of listeners) listener();
    },
    resubscribing() {
      seen = false;
    },
    synchronized: () => seen,
    sequence: () => lastSequence,
    run: () => anchor,
    activeRun: active,
    latestRun: () => last,
    busy: () => earlier.size > 0,
    empty: () => !hasMessages,
    quiet,
    providerSession() {
      if (last?.providerThreadId == null) return undefined;
      const id = providerThreads.get(last.providerThreadId)?.providerSessionId;
      return id === null || id === undefined || detachedSessions.has(id) ? undefined : id;
    },
    requestOpen: () => ownedPendingRequest() !== undefined,
    outcome() {
      if (anchor === undefined) {
        if (followUp !== undefined && seen)
          return {
            kind: "failed",
            explanation:
              "T3 no longer lists the message whose delegated work this turn reports, so its report cannot be followed",
          };
        return undefined;
      }
      if (last === undefined) {
        // Only a follow-up turn holds no run of its own: it waits for a
        // continuation as long as the delegated work still owes one.
        if (followUp === undefined || reportOwed()) return undefined;
        return {
          kind: "failed",
          explanation: "T3's delegated work ended without reporting back to this conversation",
        };
      }
      const request = ownedPendingRequest();
      if (request !== undefined) return { kind: "blocked", asked: asked(request) };
      if (uncertainFollowUp())
        return {
          kind: "unresolved",
          explanation:
            "An earlier turn on this conversation was not confirmed stopped, and T3 cannot show whether the run that followed this turn's background work reports this turn's work or the earlier one's",
        };
      if (active() !== undefined) return undefined;
      if (stray !== undefined && last.status === "completed")
        return {
          kind: "unresolved",
          explanation:
            "A run followed this turn's while Carillon was not watching, and T3's state cannot show whether it reports this turn's background work",
        };
      switch (last.status) {
        case "completed": {
          if (roster(last).length > 0) {
            if (sessionStopped(last))
              return {
                kind: "failed",
                explanation:
                  "T3's native session stopped while it still listed background work, which can no longer report back.",
              };
            if (sessionDetached(last))
              return {
                kind: "failed",
                explanation:
                  "T3's native session was detached while it still listed background work; T3 does not confirm whether that work still reports back.",
              };
            return undefined;
          }
          if (wakeOwedAfter !== undefined && last.ordinal <= wakeOwedAfter) {
            if (sessionStopped(last))
              return {
                kind: "failed",
                explanation:
                  "T3's native session stopped before it reported the result of its finished background work.",
              };
            if (sessionDetached(last))
              return {
                kind: "failed",
                explanation:
                  "T3's native session was detached before it reported the result of its finished background work; T3 does not confirm whether that result will still arrive.",
              };
            return undefined;
          }
          if (ambiguous())
            return {
              kind: "unresolved",
              explanation:
                "Carillon lost its connection to T3 while this turn ran background work, and T3's state does not show whether the result was delivered",
            };
          // The answer is complete; delegated work it started may still owe
          // the conversation a report, which a follow-up turn delivers.
          return reportOwed()
            ? {
                kind: "completed",
                output: answer(),
                followUp: {
                  message: messageId,
                  after: followUp === undefined ? anchor.ordinal : last.ordinal,
                },
              }
            : { kind: "completed", output: answer() };
        }
        case "failed":
          return { kind: "failed", explanation: errors.get(last.id) ?? "The T3 run failed" };
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
      const run = active() ?? last;
      // A child agent shows once, as its delegation, not also as a tool or a
      // background task of its parent.
      const shownAsWork = (item: ToolItem) =>
        item.type === "subagent" &&
        item.subagentId !== undefined &&
        delegations.has(item.subagentId);
      const childThreads = new Set(
        [...delegations.values()].flatMap((task) =>
          task.childThreadId === null ? [] : [task.childThreadId],
        ),
      );
      const runningTools: RunningTool[] = [...running.values()]
        .filter((item) => !shownAsWork(item))
        .map((item) => ({
          ...tool(item),
          started_at: seconds(item.startedAt ?? item.updatedAt),
        }));
      const now = Math.floor(Date.now() / 1000);
      const tasks = roster(run).filter(
        (task) => task.childThreadId === undefined || !childThreads.has(task.childThreadId),
      );
      for (const task of tasks) {
        if (!backgroundSince.has(task.taskId)) backgroundSince.set(task.taskId, now);
        runningTools.push({
          tool: toolName(task.kind ?? "background_task"),
          ...withDetail(task.description === undefined ? undefined : displayed(task.description)),
          started_at: backgroundSince.get(task.taskId) ?? now,
        });
      }
      for (const id of backgroundSince.keys())
        if (!tasks.some((task) => task.taskId === id)) backgroundSince.delete(id);
      const headline = latestAssistant === undefined ? undefined : commentary(latestAssistant.text);
      const work = workItems();
      return {
        phase:
          run === undefined && followUp !== undefined
            ? "waiting"
            : phase(run, compacting.size > 0, retrying.size > 0, runningTools.length),
        ...(headline === undefined ? {} : { headline }),
        running: runningTools.slice(0, RUNNING_CALLS),
        recent: recent
          .filter((item) => !shownAsWork(item))
          .map(
            (item): FinishedTool => ({
              ...tool(item),
              ok: item.status === "completed",
            }),
          ),
        tool_calls: counted.size,
        ...(work.length === 0 && unconfirmed.size === 0
          ? {}
          : { work: summarizeWork(work, unconfirmed.size > 0) }),
      };
    },
    retained: () => ({
      runs: turnRuns.size + earlier.size,
      running: running.size,
      recent: recent.length,
      counted: counted.size,
      requests: requestItems.size + pendingRequests.size,
      messages: assistant.size,
      providerThreads: providerThreads.size,
      sessions: sessions.size,
      pendingProviderRecords: recentThreads.size + recentSessions.size,
      detachedSessions: detachedSessions.size,
      delegations: delegations.size,
      transfers: transfers.size,
    }),
    onChange(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

function phase(
  run: Run | undefined,
  compacting: boolean,
  retrying: boolean,
  runningTools: number,
): ActivityPhase {
  if (run?.status === "queued") return "queued";
  if (run === undefined || BEFORE_PROVIDER.has(run.status)) return "starting";
  if (compacting) return "compacting";
  if (retrying) return "retrying";
  if (runningTools > 0) return "tools";
  return run.status === "waiting" ? "waiting" : "model";
}

const TASK_STATE: Readonly<Record<Subagent["status"], WorkItem["state"]>> = {
  idle: "queued",
  pending: "queued",
  running: "running",
  waiting: "waiting",
  completed: "succeeded",
  failed: "failed",
  cancelled: "cancelled",
  interrupted: "cancelled",
};

/** What a transfer's native status confirms: prepared is not consumed. */
function transferState(status: string): WorkItem["state"] {
  switch (status) {
    case "pending":
      return "running";
    case "resolved_native":
    case "resolved_portable":
      return "waiting";
    case "consumed":
      return "succeeded";
    case "failed":
      return "failed";
    case "superseded":
      return "cancelled";
    default:
      return "unknown";
  }
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
