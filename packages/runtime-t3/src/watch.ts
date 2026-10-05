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
import type { ActivityPhase, FinishedTool, RunningTool, TurnActivity } from "@carillon/protocol";
import {
  commandHeader,
  commentary,
  describe,
  displayed,
  toolName,
} from "@carillon/runtime/activity";
import { bounded } from "@carillon/runtime/failure";
import { isAbsolute, relative, resolve } from "node:path";
import type { Static, TSchema } from "typebox";
import { Check } from "typebox/value";
import {
  MessageSchema,
  ProjectionSchema,
  ProviderSessionDetachedSchema,
  ProviderSessionSchema,
  ProviderThreadSchema,
  RunSchema,
  RuntimeRequestSchema,
  TurnItemSchema,
  TurnItemSchemas,
  type BackgroundTask,
  type Message,
  type ProviderSession,
  type ProviderThread,
  type Run,
  type RunStatus,
  type RuntimeRequest,
  type TurnItemOf,
} from "./contracts.ts";
import { T3Failure } from "./t3.ts";

export type Outcome =
  | { readonly kind: "completed"; readonly output: string }
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
  /** Earlier turns' runs that still occupy the thread. */
  blocking(): readonly Run[];
  /**
   * While the turn has no run: the thread's latest earlier run and the
   * background work its provider thread still lists, which T3 settles
   * through that run. Undefined once the turn's run exists.
   */
  stale(): { readonly run: Run; readonly listed: readonly BackgroundTask[] } | undefined;
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

export interface WatchOptions {
  /**
   * An earlier turn on this thread ended with a stop T3 did not confirm.
   * Its process may still deliver a wake, and T3 labels every wake alike, so
   * no follow-up of this turn can be trusted as its own: the turn ends
   * unresolved as soon as one is adopted. The runtime owns this fact.
   */
  readonly uncertainPredecessor?: boolean;
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
  /** Earlier runs still active, which block a new dispatch. */
  let earlier = new Map<string, Run>();
  /**
   * While the turn has no run: the thread's latest earlier run and its
   * provider thread, whose listed background work outlives the run and
   * must be settled before a successor is dispatched. Released once the
   * turn's own run exists.
   */
  let predecessor: Run | undefined;
  let predecessorThread: ProviderThread | undefined;
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
    predecessor = undefined;
    predecessorThread = undefined;
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
    if (foreign.has(run.id)) return false;
    if (anchor.status !== "completed") {
      foreign.add(run.id);
      return false;
    }
    if (wakeOwedAfter !== undefined && run.ordinal > wakeOwedAfter) return true;
    if (backgroundSeenAt !== undefined) return true;
    stray ??= run.id;
    return false;
  }

  function foldRun(run: Run): void {
    if (run.userMessageId === messageId) {
      anchor = run;
      predecessor = undefined;
      predecessorThread = undefined;
    }
    const owned = anchor !== undefined && (run.id === anchor.id || adopt(run));
    if (owned) {
      turnRuns.set(run.id, run);
      earlier.delete(run.id);
      if (last === undefined || run.ordinal >= last.ordinal) last = run;
      return;
    }
    if (anchor !== undefined && run.ordinal > anchor.ordinal) return;
    if (isActive(run)) earlier.set(run.id, run);
    else earlier.delete(run.id);
    if (anchor === undefined && (predecessor === undefined || run.ordinal >= predecessor.ordinal)) {
      predecessor = run;
      predecessorThread =
        run.providerThreadId === null
          ? undefined
          : (recentThreads.get(run.providerThreadId) ?? providerThreads.get(run.providerThreadId));
    }
  }

  /** An adopted follow-up whose provenance an unconfirmed earlier stop leaves uncertain. */
  function uncertainFollowUp(): boolean {
    if (options.uncertainPredecessor !== true || anchor === undefined) return false;
    for (const run of turnRuns.values()) if (run.ordinal > anchor.ordinal) return true;
    return false;
  }

  /** Runs seen before the turn's own were filed as earlier; refile those from the turn onward. */
  function refile(): void {
    if (anchor === undefined) return;
    for (const run of [...earlier.values()]) if (run.ordinal >= anchor.ordinal) foldRun(run);
  }

  function foldMessage(message: Message): void {
    hasMessages = true;
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
    if (predecessor?.providerThreadId === thread.id) predecessorThread = thread;
    if (turnProviderThread(thread.id)) {
      providerThreads.set(thread.id, thread);
      pruneProviders();
    } else remember(recentThreads, thread);
  }

  function foldSession(session: ProviderSession): void {
    if (turnSession(session.id)) {
      sessions.set(session.id, session);
      detachedSessions.delete(session.id);
    } else remember(recentSessions, session);
  }

  function remember<T extends { readonly id: string }>(recent: Map<string, T>, record: T): void {
    recent.delete(record.id);
    recent.set(record.id, record);
    for (const id of recent.keys()) {
      if (recent.size <= RECENT_RECORDS) break;
      recent.delete(id);
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
          // Runs in ordinal order, so the turn's own is known before the
          // runs that follow it are judged.
          for (const run of [...projection.runs].sort((a, b) => a.ordinal - b.ordinal))
            foldRun(run);
          refile();
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
    blocking: () => [...earlier.values()],
    stale: () =>
      predecessor === undefined
        ? undefined
        : {
            run: predecessor,
            listed:
              predecessor.providerThreadId === null ? [] : listed(predecessorThread, predecessor),
          },
    empty: () => !hasMessages,
    quiet,
    providerSession() {
      if (last?.providerThreadId == null) return undefined;
      const id = providerThreads.get(last.providerThreadId)?.providerSessionId;
      return id === null || id === undefined || detachedSessions.has(id) ? undefined : id;
    },
    requestOpen: () => ownedPendingRequest() !== undefined,
    outcome() {
      if (anchor === undefined || last === undefined) return undefined;
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
          return { kind: "completed", output: answer() };
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
      const runningTools: RunningTool[] = [...running.values()].map((item) => ({
        ...tool(item),
        started_at: seconds(item.startedAt ?? item.updatedAt),
      }));
      const now = Math.floor(Date.now() / 1000);
      const tasks = roster(run);
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
      return {
        phase: phase(run, compacting.size > 0, retrying.size > 0, runningTools.length),
        ...(headline === undefined ? {} : { headline }),
        running: runningTools.slice(0, RUNNING_CALLS),
        recent: recent.map(
          (item): FinishedTool => ({
            ...tool(item),
            ok: item.status === "completed",
          }),
        ),
        tool_calls: counted.size,
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
  if (run === undefined || BEFORE_PROVIDER.has(run.status)) return "starting";
  if (compacting) return "compacting";
  if (retrying) return "retrying";
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
