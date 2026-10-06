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
 * The reasoning T3 publishes for the turn's runs becomes its notes; a call's
 * start is T3's own, or when the fold first saw it, marked as an estimate.
 * Nothing here talks to the network.
 */
import type {
  ActivityPhase,
  AgentInfo,
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
  keepNote,
  summarizeWork,
  toolName,
} from "@carillon/runtime/activity";
import { bounded } from "@carillon/runtime/failure";
import { isAbsolute, relative, resolve } from "node:path";
import type { Static, TSchema } from "typebox";
import { Check } from "typebox/value";
import {
  ContextTransferSchema,
  DelegatedNotificationSchema,
  MessageSchema,
  ProviderSessionDetachedSchema,
  ProviderSessionSchema,
  ProviderThreadSchema,
  RunSchema,
  RuntimeRequestSchema,
  SubagentSchema,
  ThreadSnapshotSchema,
  TurnItemSchema,
  TurnItemSchemas,
  WakeNotificationSchema,
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
  /** Calls under way: running, pending or waiting. */
  readonly running: number;
  readonly recent: number;
  readonly counted: number;
  readonly requests: number;
  readonly messages: number;
  readonly providerThreads: number;
  readonly sessions: number;
  /** Provider records and transfers held until the run they belong to is known. */
  readonly pendingProviderRecords: number;
  readonly detachedSessions: number;
  readonly delegations: number;
  readonly transfers: number;
  /** Continuation links: the turn's, and the few most recent of other runs. */
  readonly links: number;
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
  /** Whether runs this turn does not own still occupy the thread: a new message waits behind them. */
  busy(): boolean;
  /**
   * Whether the provider session is not the turn's alone to stop: a run this
   * turn does not own is active, or background work another run started is
   * still listed.
   */
  sessionShared(): boolean;
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
/** Run statuses before T3 sends the run's opening item. */
const UNOPENED: ReadonlySet<RunStatus> = new Set(["preparing", "queued"]);
const RUNNING_ITEM: ReadonlySet<string> = new Set(["pending", "running", "waiting"]);
/** A call T3 has asked for or holds without showing it executing. */
const HELD_CALL: ReadonlySet<string> = new Set(["pending", "waiting"]);
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
/** A result T3 delivered to the parent in a continuation. */
const DELIVERED: ReadonlySet<string> = new Set(["delivered", "acknowledged"]);
/** What T3's records prove when it gave a result to a run started by another message. */
const STEERED_REPORT =
  "T3 gave the result of the delegated work to the agent during a later message's run in this conversation, not in a report of its own; T3 shows it.";
/** What T3's records prove when an agent read the result in a run that is not this report's. */
const READ_ELSEWHERE =
  "The agent read the result of the delegated work in another run of this conversation, not in a report of its own; T3 shows it.";
/** What T3's records prove when an agent read the result and T3 names no run it was read in. */
const ACKNOWLEDGED =
  "T3 records the result of the delegated work as acknowledged, so no report of its own follows here; T3 shows it.";
const OLDER_REPORT =
  "T3 finished the report of the delegated work, but it is older than the part of the thread this runtime reads; read it in T3";
/** Transfers shown as work of their own; spawn and result transfers belong to their delegation. */
const TRANSFER_WORK: ReadonlyMap<string, { kind: WorkItem["kind"]; label: string }> = new Map([
  ["provider_handoff", { kind: "handoff", label: "context handoff" }],
  ["fork", { kind: "fork", label: "fork to another thread" }],
  ["merge_back", { kind: "fork", label: "merge back from another thread" }],
]);
const RECENT_CALLS = 5;
/** Each provider's own option for reasoning effort: Claude's `effort`, Codex's `reasoningEffort`. */
const EFFORT_OPTIONS: ReadonlySet<string> = new Set(["effort", "reasoningEffort"]);
/**
 * Child agents (and, separately, transfers) a turn keeps records of. Beyond
 * it, settled records make room; when all are live, coverage is partial and
 * the report is owed until a snapshot shows otherwise.
 */
const WORK_RETAINED = 32;
/** Continuation links of runs not (yet) the turn's, kept for reference-before-owner order. */
const PENDING_LINKS = 16;
/** Run statuses during which a run may start tools, and so background work. */
const EXECUTING: ReadonlySet<RunStatus> = new Set(["running", "waiting"]);
const SETTLED_TASK: ReadonlySet<Subagent["status"]> = new Set([
  "completed",
  "failed",
  "cancelled",
  "interrupted",
]);
const SETTLED_TRANSFER: ReadonlySet<string> = new Set(["consumed", "failed", "superseded"]);
const RUNNING_CALLS = 8;
/** Provider records kept while the fold cannot yet tell whether the turn owns them. */
const RECENT_RECORDS = 4;
const KNOWN_ITEMS: ReadonlySet<string> = new Set(Object.keys(TurnItemSchemas));

/**
 * How T3 opened a run: every run's timeline begins with the message that
 * started it, a person's (`user_message`), or one T3 created itself as a
 * `notification`: a provider's wake, or a delegated task's continuation.
 * A notification whose source this runtime does not recognise is `unknown`:
 * it confers nothing.
 */
type Opening = "message" | "wake" | "delegated" | "unknown";

type ToolItem = TurnItemOf<
  "command_execution" | "file_change" | "file_search" | "web_search" | "dynamic_tool" | "subagent"
>;
type RequestItem = TurnItemOf<"user_input_request" | "approval_request">;

export function isActive(run: Run): boolean {
  return ACTIVE.has(run.status);
}

function executing(run: Run): boolean {
  return EXECUTING.has(run.status);
}

/** A run known to have ended; an unknown run is not. */
function settledRun(status: RunStatus | undefined): boolean {
  return status !== undefined && !ACTIVE.has(status);
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
  threadId: string,
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
  /**
   * Continuation messages of the turn's runs, by message, with the run whose
   * work they report. T3 links a continuation by its message's
   * `delegatedCompletion` or the parent run's announcement, and drops both
   * once it delivered the continuation; the continuation run's own leading
   * `delegated_task` notification remains, and links it through the run's
   * message. A task's `observedByRunId` is not a link: it names whichever
   * run was active when an agent read the result, which may be another
   * message's run.
   */
  const ownLinks = new Map<string, string>();
  /**
   * The few most recent continuation links of other runs: a continuation of
   * another turn's work is neither this turn's wake nor a stray, and a link
   * may name its parent before the fold knows the parent is the turn's.
   */
  const pendingLinks = new Map<string, string>();
  /** A snapshot's links while it is folded; never kept beyond it. */
  let snapshotLinks: ReadonlyMap<string, string> | undefined;
  /**
   * How the few most recent runs opened (see `Opening`), from their first
   * item. A notification inside a run a person's message opened means T3
   * delivered a result into that run instead.
   */
  const openings = new Map<string, Opening>();
  /** A snapshot's openings while it is folded; never kept beyond it. */
  let snapshotOpenings: ReadonlyMap<string, Opening> | undefined;
  /** The turn's delegated tasks whose results T3 delivered into a run started by another message. */
  const steered = new Set<string>();
  /** Later runs not (yet) known to be the turn's, kept briefly in case a continuation link arrives after them. */
  const unclaimed = new Map<string, Run>();
  /** Runs adopted because the turn's process owed a background report (wakes), not by a native link. */
  const wakeRuns = new Set<string>();
  /** Child agents started by the turn's runs or their continuations, kept across snapshots, up to the bound. */
  const delegations = new Map<string, Subagent>();
  /**
   * Delegations a resumed snapshot no longer listed: their current state is
   * not confirmed. A settled one keeps the outcome T3 confirmed.
   */
  const unconfirmed = new Set<string>();
  /** Handoffs, forks and merges of the turn's runs, up to the bound. */
  const transfers = new Map<string, ContextTransfer>();
  /** Work the bound left out: the summary is partial. */
  let workOmitted = false;
  /** Live or owed child agents the bound left out: the report counts as owed until a snapshot settles it. */
  let owedUnknown = false;
  /** Runs first seen in a bounded snapshot that may have left their messages outside its window. */
  const windowed = new Set<string>();
  /** The latest snapshot left older history outside its window. */
  let historyOmitted = false;
  /**
   * The few most recent transfers that name no run the turn holds yet: T3
   * records a provider handoff before it creates the run it hands over to.
   */
  const recentTransfers = new Map<string, ContextTransfer>();
  let hasMessages = false;
  /** The turn's assistant messages by ID, kept across snapshots. */
  const assistant = new Map<string, Message>();
  let latestAssistant: Message | undefined;
  let running = new Map<string, ToolItem>();
  /** Calls pending or waiting: neither executing nor finished. */
  let held = new Set<string>();
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
   * Later runs that are never this turn's: first seen while its run was
   * still active (this turn's own wake can only start after its run
   * completed with work still listed, so such a run is another process's),
   * or opened by a person's message or a delegated task's continuation.
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
  /**
   * Background work first listed while none of the turn's runs could have
   * been executing: another run's, such as one started in T3's own UI or an
   * earlier turn's, whose report is not this turn's to wait for or answer
   * with. Work first listed while one of them could have run, or across a
   * gap the fold did not see, is the turn's. Both sets keep only what the
   * turn's provider threads still list.
   */
  const ownTasks = new Set<string>();
  const otherTasks = new Set<string>();
  const listeners = new Set<() => void>();
  /** When each background task was first seen, since T3 does not date them. */
  const backgroundSince = new Map<string, number>();
  /**
   * When the fold first saw each call running, kept across snapshots and
   * waits: the start of a call T3 gives none for, and the evidence that a
   * held call ran.
   */
  const firstSeen = new Map<string, number>();
  /** The reasoning T3 published for the turn's runs, by item, kept across snapshots. */
  const notes = new Map<string, string>();

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
    held = new Set();
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
    recentTransfers.clear();
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
    if (turnRuns.has(run.id)) {
      // An adopted continuation's record follows its run, so a link whose
      // continuation ran can make room for another (see `keepLink`).
      if (continuationRuns.has(run.id)) continuationRuns.set(run.id, run);
      return true;
    }
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
    // Another run's continuation is never this turn's wake or a stray.
    if (linkParent(run.userMessageId) !== undefined) return false;
    if (foreign.has(run.id)) return false;
    // A follow-up turn owns only its continuations: with the conversation's
    // later messages running beside it, no other run can be attributed to it.
    if (followUp !== undefined) return false;
    if (anchor.status !== "completed") {
      foreign.add(run.id);
      return false;
    }
    // Only a run T3 opened with a provider's notification can be this
    // turn's wake: a run a person's message opened, or a delegated task's
    // continuation, is another's. A run whose opening is not seen yet is not
    // judged: live, its opening follows it, and a queued run gets its opening
    // when it starts. A run opened by a notification this runtime cannot
    // identify, or that a snapshot shows started without its opening, cannot
    // be attributed.
    const opening = openings.get(run.id) ?? snapshotOpenings?.get(run.id);
    if (opening === "message" || opening === "delegated") {
      foreign.add(run.id);
      return false;
    }
    if (opening === undefined) {
      if (snapshotOpenings !== undefined && !UNOPENED.has(run.status)) stray ??= run.id;
      return false;
    }
    if (opening === "unknown") {
      stray ??= run.id;
      return false;
    }
    // A wake names no work: it is the provider process's that runs it. Only
    // a wake on the provider thread whose roster held this turn's work can
    // report it; one on another provider thread is another process's. A
    // queued wake's provider thread is known only once it starts.
    if (run.providerThreadId === null) return false;
    if (run.providerThreadId !== last?.providerThreadId) {
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

  /** Whether T3 linked this run to work of a run the turn holds. */
  function continuationOf(run: Run): boolean {
    const parent = linkParent(run.userMessageId);
    return parent !== undefined && linkedRun(parent);
  }

  /** The run whose delegated task a notification reports, with the turn's tasks it names. */
  function notifiedParent(
    payload: unknown,
    parentOf: (task: string) => string | null | undefined,
  ):
    | { readonly run: string; readonly parent: string; readonly tasks: readonly string[] }
    | undefined {
    if (!Check(DelegatedNotificationSchema, payload)) return undefined;
    const tasks = payload.source.taskIds.filter((task) => parentOf(task) != null);
    const parent = tasks.length === 0 ? undefined : parentOf(tasks[0] ?? "");
    return parent == null ? undefined : { run: payload.runId, parent, tasks };
  }

  /**
   * A notification of the turn's delegated work: a continuation of it when
   * the run starts with the notification, a delivery into another message's
   * run when that run started with a message.
   */
  function notified(
    report: { readonly run: string; readonly parent: string; readonly tasks: readonly string[] },
    startedByMessage: boolean,
  ): void {
    if (!linkedRun(report.parent)) return;
    if (!startedByMessage) {
      // T3 sends a run before its items, so the run is among those seen.
      const run = [...unclaimed.values()].find((candidate) => candidate.id === report.run);
      if (run !== undefined) link(run.userMessageId, report.parent);
      return;
    }
    for (const task of report.tasks) if (steered.size < WORK_RETAINED) steered.add(task);
  }

  /** The run whose delegated work a continuation message reports, if T3 has said. */
  function linkParent(message: string): string | undefined {
    return ownLinks.get(message) ?? snapshotLinks?.get(message) ?? pendingLinks.get(message);
  }

  /** Keeps a continuation link: the turn's for good (up to the bound), another run's briefly. */
  function keepLink(message: string, parentRun: string): void {
    if (!linkedRun(parentRun)) {
      remember(pendingLinks, message, parentRun, PENDING_LINKS);
      return;
    }
    pendingLinks.delete(message);
    if (!ownLinks.has(message) && ownLinks.size >= WORK_RETAINED) {
      // Room is made by a link whose continuation already ran; a link still
      // owed is never dropped for a newer one.
      const ran = new Set(
        [...continuationRuns.values()]
          .filter((run) => !isActive(run))
          .map((run) => run.userMessageId),
      );
      const room = [...ownLinks.keys()].find((kept) => ran.has(kept));
      if (room === undefined) {
        workOmitted = true;
        owedUnknown = true;
        return;
      }
      ownLinks.delete(room);
    }
    ownLinks.set(message, parentRun);
  }

  /** A run whose delegated work and continuations belong to this turn's report. */
  function linkedRun(id: string): boolean {
    return anchor?.id === id || turnRuns.has(id) || continuationRuns.has(id);
  }

  /** Records a continuation link, and folds again a run it arrived after. */
  function link(message: string, parentRun: string): void {
    if (linkParent(message) === parentRun && (ownLinks.has(message) || !linkedRun(parentRun)))
      return;
    keepLink(message, parentRun);
    const run = unclaimed.get(message);
    if (run !== undefined) refold(run);
  }

  /** Folds again a run a link arrived after, adopting the provider records it names. */
  function refold(run: Run): void {
    foldRun(run);
    pruneProviders();
  }

  function foldRun(run: Run): void {
    if (run.userMessageId === messageId) anchor = run;
    const owned =
      anchor !== undefined && ((run.id === anchor.id && followUp === undefined) || adopt(run));
    // The run names the continuation that will report its delegated work. A
    // snapshot's announcements are collected before its runs are folded and
    // kept after, once its continuations are known.
    const delivery = snapshotLinks === undefined ? run.delegatedCompletion?.delivery : undefined;
    if (owned) {
      turnRuns.set(run.id, run);
      earlier.delete(run.id);
      if (last === undefined || run.ordinal >= last.ordinal) last = run;
      for (const [message, parent] of [...pendingLinks])
        if (parent === run.id) keepLink(message, parent);
      if (delivery != null) keepLink(delivery.messageId, run.id);
      for (const transfer of [...recentTransfers.values()])
        if (namesLinkedRun(transfer)) foldTransfer(transfer);
      return;
    }
    if (delivery != null) keepLink(delivery.messageId, run.id);
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
    if (owedUnknown) return true;
    for (const task of delegations.values())
      if (
        task.origin === "app_owned" &&
        (ACTIVE_TASK.has(task.status) || OWED_DELIVERY.has(task.completionDelivery?.state ?? ""))
      )
        return true;
    const started = new Set([...continuationRuns.values()].map((run) => run.userMessageId));
    for (const [message, parent] of ownLinks)
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
    // The turn's answer is ordered by when T3 made each message: one whose
    // time cannot be read cannot be placed.
    if (Number.isNaN(madeAt(message)))
      throw new T3Failure(
        "malformed",
        "T3 sent an assistant message this runtime does not understand",
      );
    assistant.set(message.id, message);
    if (
      latestAssistant === undefined ||
      latestAssistant.id === message.id ||
      madeAt(message) >= madeAt(latestAssistant)
    )
      latestAssistant = message;
  }

  function foldItem(payload: unknown): void {
    if (!isRecord(payload)) return;
    const type = payload["type"];
    const runId = payload["runId"];
    const opening = openingOf(payload);
    if (opening !== undefined) {
      const [run, kind] = opening;
      const startedByMessage = openings.get(run) === "message";
      if (!openings.has(run)) remember(openings, run, kind, PENDING_LINKS);
      const report =
        type === "notification"
          ? notifiedParent(payload, (task) => delegations.get(task)?.runId)
          : undefined;
      if (report !== undefined) notified(report, startedByMessage);
      // A later run waits for its opening before it is judged.
      const waiting = [...unclaimed.values()].find((candidate) => candidate.id === run);
      if (waiting !== undefined) refold(waiting);
      return;
    }
    // Only the turn's own items of the kinds this runtime reads are checked and kept.
    if (typeof type !== "string" || !KNOWN_ITEMS.has(type)) return;
    // Such an item names its run or none; naming it any other way, it cannot
    // be passed over as another run's.
    if (runId !== null && (typeof runId !== "string" || runId.length === 0))
      throw new T3Failure("malformed", `T3 sent a ${type} item this runtime does not understand`);
    if (runId === null || !turnRuns.has(runId)) return;
    const item = parsed(TurnItemSchema, payload, `${type} item`);
    if (item.runId === null) return;
    switch (item.type) {
      case "command_execution":
      case "file_change":
      case "file_search":
      case "web_search":
      case "dynamic_tool":
      case "subagent":
        // Only a running call is executing: one pending or waiting is held,
        // and counted and timed once T3 shows it running or finished. A held
        // call the fold saw running stays counted, even after a snapshot.
        if (item.status === "running") {
          counted.add(item.id);
          held.delete(item.id);
          running.set(item.id, item);
          if (!firstSeen.has(item.id)) firstSeen.set(item.id, Math.floor(Date.now() / 1000));
          return;
        }
        running.delete(item.id);
        if (HELD_CALL.has(item.status)) {
          held.add(item.id);
          if (firstSeen.has(item.id)) counted.add(item.id);
          return;
        }
        held.delete(item.id);
        firstSeen.delete(item.id);
        if (FINISHED_ITEM.has(item.status)) {
          counted.add(item.id);
          recent = recent.filter((finished) => finished.id !== item.id);
          recent.push(item);
          if (recent.length > RECENT_CALLS) recent.shift();
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
      case "reasoning":
        keepNote(notes, item.id, item.text);
        return;
    }
  }

  /** A child agent of a run the turn holds; others are another conversation's business. */
  function foldSubagent(task: Subagent): void {
    if (task.runId === null || !linkedRun(task.runId)) return;
    if (retain(delegations, task.id, task, settledTask)) unconfirmed.delete(task.id);
    else {
      workOmitted = true;
      if (!settledTask(task)) owedUnknown = true;
    }
  }

  /** Settled: finished, with nothing left for T3 to deliver. */
  function settledTask(task: Subagent): boolean {
    return (
      SETTLED_TASK.has(task.status) && !OWED_DELIVERY.has(task.completionDelivery?.state ?? "")
    );
  }

  /**
   * Keeps a record within the bound, making room by dropping the oldest
   * settled record; false when every kept record is still live.
   */
  function retain<T>(
    records: Map<string, T>,
    id: string,
    record: T,
    settled: (record: T) => boolean,
  ): boolean {
    if (!records.has(id) && records.size >= WORK_RETAINED) {
      const room = [...records].find(([, kept]) => settled(kept));
      if (room === undefined) return false;
      records.delete(room[0]);
      unconfirmed.delete(room[0]);
      workOmitted = true;
    }
    records.set(id, record);
    return true;
  }

  function foldTransfer(transfer: ContextTransfer): void {
    if (!TRANSFER_WORK.has(transfer.type)) return;
    if (namesLinkedRun(transfer)) {
      recentTransfers.delete(transfer.id);
      if (!retain(transfers, transfer.id, transfer, (kept) => SETTLED_TRANSFER.has(kept.status)))
        workOmitted = true;
    } else remember(recentTransfers, transfer.id, transfer);
  }

  function namesLinkedRun(transfer: ContextTransfer): boolean {
    return [transfer.sourcePoint.runId, transfer.targetRunId].some(
      (run) => run != null && linkedRun(run),
    );
  }

  function foldRequest(request: RuntimeRequest): void {
    if (request.status === "pending") pendingRequests.set(request.id, request);
    else pendingRequests.delete(request.id);
  }

  /**
   * The turn's background work on a run's provider thread: what it lists,
   * less work another run started (see `otherTasks`). T3 sends the thread
   * record before the run that names it and lists provider threads whole in
   * a snapshot, so a run that names a thread this fold has not been given
   * is a broken reference: its work is unknown, never an empty roster.
   */
  function roster(run: Run | undefined): readonly BackgroundTask[] {
    return listedWork(run).filter((task) => !otherTasks.has(task.taskId));
  }

  /** Everything the run's provider thread lists, the turn's or not. */
  function listedWork(run: Run | undefined): readonly BackgroundTask[] {
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
  function remember<T>(
    recent: Map<string, T>,
    key: string,
    record: T,
    limit = RECENT_RECORDS,
  ): void {
    recent.delete(key);
    recent.set(key, record);
    for (const oldest of recent.keys()) {
      if (recent.size <= limit) break;
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
      .sort((left, right) => madeAt(left) - madeAt(right))
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

  /** A running call's start: T3's own, or when the fold first saw it, marked as an estimate. */
  function started(item: ToolItem): { started_at: number; estimated?: true } {
    const native = Date.parse(item.startedAt ?? "");
    if (!Number.isNaN(native)) return { started_at: Math.floor(native / 1000) };
    return { started_at: firstSeen.get(item.id) ?? Math.floor(Date.now() / 1000), estimated: true };
  }

  /** What T3 says about the agent of `run`: its selection, and its provider thread's context. */
  function agentOf(run: Run | undefined): AgentInfo | undefined {
    const selection = run?.modelSelection;
    const usage =
      run?.providerThreadId == null
        ? undefined
        : providerThreads.get(run.providerThreadId)?.contextUsage;
    const effort = selection?.options?.find((option) => EFFORT_OPTIONS.has(option.id))?.value;
    const agent: AgentInfo = {
      ...(selection === undefined ? {} : { model: toolName(selection.model) }),
      ...(typeof effort === "string" ? { effort: toolName(effort) } : {}),
      ...(usage == null
        ? {}
        : {
            context: {
              used: usage.usedTokens,
              ...(usage.maxTokens === undefined ? {} : { window: usage.maxTokens }),
            },
          }),
    };
    return Object.keys(agent).length === 0 ? undefined : agent;
  }

  function seconds(stamp: string | null): number {
    const parsed = Date.parse(stamp ?? "");
    return Number.isNaN(parsed) ? Math.floor(Date.now() / 1000) : Math.floor(parsed / 1000);
  }

  /** The run, refused when it is another thread's: T3 streams one thread's runs. */
  function ofThread(run: Run): Run {
    if (run.threadId !== threadId)
      throw new T3Failure("malformed", `T3 sent run ${run.id} of another thread`);
    return run;
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
        // A settled outcome T3 confirmed stands when a later snapshot omits
        // the task; only live state becomes unknown. The summary is partial.
        state:
          unconfirmed.has(task.id) && !SETTLED_TASK.has(task.status)
            ? "unknown"
            : TASK_STATE[task.status],
        ...(delegations.has(task.parentNodeId) ? { parent: task.parentNodeId } : {}),
        ...(actor.length === 0 ? {} : { actor: toolName(actor) }),
        ...(task.startedAt === null ? {} : { started_at: seconds(task.startedAt) }),
        ...(SETTLED_TASK.has(task.status) ? lasted(task.startedAt, task.completedAt) : {}),
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

  /**
   * Where results went when T3's records show them given to a run that is
   * not one of this report's continuations: delivered into a run another
   * message started, or read by an agent in another run. An acknowledgement
   * that names no run says only that the result was read.
   */
  function elsewhere(): string[] {
    let readElsewhere = false;
    let acknowledged = false;
    for (const task of delegations.values()) {
      const delivery = task.completionDelivery;
      if (delivery?.state !== "acknowledged" || steered.has(task.id)) continue;
      const observer = delivery.observedByRunId;
      if (observer == null) acknowledged = true;
      else if (!turnRuns.has(observer)) readElsewhere = true;
    }
    return [
      ...(steered.size > 0 ? [STEERED_REPORT] : []),
      ...(readElsewhere ? [READ_ELSEWHERE] : []),
      ...(acknowledged ? [ACKNOWLEDGED] : []),
    ];
  }

  /**
   * A follow-up turn's outcome once its latest continuation completed: the
   * continuations' text, carrying another cursor while delegated work still
   * owes a report. Text a resumed bounded snapshot may have left outside its
   * window is reported missing, never passed off as the whole report.
   */
  function reported(latest: Run): Outcome {
    const output = answer();
    const missing = [...turnRuns.keys()].some(
      (id) => windowed.has(id) && ![...assistant.values()].some((message) => message.runId === id),
    );
    if (output.length === 0 && missing) return { kind: "failed", explanation: OLDER_REPORT };
    const notes = [
      ...elsewhere().map((note) => `[${note}]`),
      ...(missing
        ? [
            "[Part of this report is older than the part of the thread Carillon reads; T3 shows all of it.]",
          ]
        : []),
      ...(roster(latest).length > 0 ||
      (wakeOwedAfter !== undefined && latest.ordinal <= wakeOwedAfter)
        ? ["[The report started background work in T3; its result will show in T3, not here.]"]
        : []),
    ];
    const text = [output, ...notes].filter((part) => part.length > 0).join("\n\n");
    return reportOwed()
      ? {
          kind: "completed",
          output: text,
          followUp: { message: messageId, after: latest.ordinal },
        }
      : { kind: "completed", output: text };
  }

  /** One of the turn's runs is executing now. */
  function ownRunExecuting(): boolean {
    for (const run of turnRuns.values()) if (executing(run)) return true;
    return false;
  }

  /** Later runs on the thread this turn does not own, as last seen. */
  function laterOthers(): Run[] {
    return [...unclaimed.values(), ...continuationRuns.values()].filter(
      (run) => !turnRuns.has(run.id),
    );
  }

  /**
   * Files background work the turn's provider threads list but the fold has
   * not classified yet, and forgets what they no longer list.
   */
  function classifyWork(ownCouldHaveRun: boolean): void {
    const listedNow = new Set<string>();
    for (const thread of providerThreads.values())
      for (const task of thread.pendingBackgroundTasks ?? []) {
        listedNow.add(task.taskId);
        if (ownTasks.has(task.taskId) || otherTasks.has(task.taskId)) continue;
        (ownCouldHaveRun ? ownTasks : otherTasks).add(task.taskId);
      }
    for (const id of ownTasks) if (!listedNow.has(id)) ownTasks.delete(id);
    for (const id of otherTasks) if (!listedNow.has(id)) otherTasks.delete(id);
  }

  function busy(): boolean {
    return earlier.size > 0 || laterOthers().some(isActive);
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
          if (!Check(ThreadSnapshotSchema, item))
            throw new T3Failure(
              "malformed",
              "T3 sent a thread snapshot this runtime does not understand",
            );
          const { projection, snapshotSequence: sequence, hasMoreHistory } = item;
          // Nothing of another thread is folded as this turn's.
          if (projection.thread.id !== threadId)
            throw new T3Failure("malformed", "T3 sent the snapshot of another thread");
          for (const run of projection.runs) ofThread(run);
          // A snapshot lists the thread's runs whole: one that omits a run
          // this turn already observed, its own or a follow-up's parent, is
          // broken, and is refused before it replaces anything the fold knows.
          const observed = new Set(turnRuns.keys());
          if (anchor !== undefined) observed.add(anchor.id);
          for (const id of observed)
            if (!projection.runs.some((run) => run.id === id))
              throw new T3Failure(
                "malformed",
                `T3 sent a thread snapshot that omits run ${id} this turn observed`,
              );
          const before = new Map([...turnRuns].map(([id, run]) => [id, run.status]));
          reset();
          // A snapshot lists runs, announcements and children whole, so the
          // coverage of the turn's work is decided afresh from it.
          workOmitted = false;
          owedUnknown = false;
          // Continuation links first, from messages, the runs that announce
          // them and the notifications continuation runs begin with, so no
          // continuation is ever judged a stray; then runs in
          // ordinal order, so the turn's own is known before the runs that
          // follow it are judged. Only the turn's links, and the few most
          // recent others, outlive the snapshot.
          const links = new Map<string, string>();
          for (const message of projection.messages)
            if (message.delegatedCompletion !== undefined)
              links.set(message.id, message.delegatedCompletion.parentRunId);
          for (const run of projection.runs)
            if (run.delegatedCompletion?.delivery != null)
              links.set(run.delegatedCompletion.delivery.messageId, run.id);
          const taskParents = new Map(projection.subagents.map((task) => [task.id, task.runId]));
          const runMessages = new Map(projection.runs.map((run) => [run.id, run.userMessageId]));
          const opened = new Map<string, Opening>();
          for (const payload of projection.turnItems) {
            const opening = openingOf(payload);
            if (opening !== undefined && !opened.has(opening[0])) opened.set(...opening);
          }
          const reports: { run: string; parent: string; tasks: readonly string[] }[] = [];
          for (const payload of projection.turnItems) {
            const report = notifiedParent(payload, (task) => taskParents.get(task));
            if (report === undefined) continue;
            if (opened.get(report.run) === "message") reports.push(report);
            else {
              const message = runMessages.get(report.run);
              if (message !== undefined) links.set(message, report.parent);
            }
          }
          snapshotLinks = links;
          snapshotOpenings = opened;
          try {
            for (const run of [...projection.runs].sort((a, b) => a.ordinal - b.ordinal))
              foldRun(run);
            refile();
          } finally {
            snapshotLinks = undefined;
            snapshotOpenings = undefined;
          }
          for (const [message, parent] of links) keepLink(message, parent);
          for (const report of reports) notified(report, true);
          // Only `hasMoreHistory` says T3 left history out of the snapshot.
          const windowedSnapshot = hasMoreHistory === true;
          historyOmitted = windowedSnapshot;
          for (const run of turnRuns.values())
            if (windowedSnapshot && !before.has(run.id)) windowed.add(run.id);
          // Delegations and transfers arrive whole: the turn's coverage is
          // decided afresh. One the turn knew that this snapshot omits keeps
          // what T3 last confirmed, its current state unconfirmed; live and
          // owed work is kept before settled work.
          const listed = projection.subagents.filter(
            (task) => task.runId !== null && linkedRun(task.runId),
          );
          const listedIds = new Set(listed.map((task) => task.id));
          for (const id of delegations.keys()) if (!listedIds.has(id)) unconfirmed.add(id);
          for (const task of [...listed].sort(
            (left, right) => Number(settledTask(left)) - Number(settledTask(right)),
          ))
            foldSubagent(task);
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
          // Background work listed now that the fold has not classified was
          // started across the gap: the turn's if one of its runs could have
          // executed in it.
          classifyWork(
            [...turnRuns.values()].some(
              (run) =>
                executing(run) ||
                (!BEFORE_PROVIDER.has(run.status) && !settledRun(before.get(run.id))),
            ),
          );
          lastSequence = sequence;
          seen = true;
          break;
        }
        case "event": {
          const executingBefore = ownRunExecuting();
          const event = item["event"];
          const sequence = item["sequence"];
          if (!isRecord(event) || typeof event["type"] !== "string" || !isSequence(sequence))
            throw new T3Failure(
              "malformed",
              "T3 sent a thread event this runtime does not understand",
            );
          // A resumed subscription may replay events already folded. The
          // cursor moves past an event only once it is folded or passed over,
          // never past one refused.
          if (lastSequence !== undefined && sequence <= lastSequence) return;
          const payload = event["payload"];
          switch (event["type"]) {
            case "run.created":
            case "run.updated":
              foldRun(ofThread(parsed(RunSchema, payload, "run")));
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
              lastSequence = sequence;
              return;
          }
          lastSequence = sequence;
          classifyWork(executingBefore || ownRunExecuting());
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
    busy,
    sessionShared: () => busy() || otherTasks.size > 0,
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
        // T3 gave the result to another run instead of a report of its own:
        // that run's reply is its own, never repeated here.
        const notes = elsewhere();
        if (notes.length > 0) return { kind: "completed", output: notes.join("\n\n") };
        // T3 says it delivered a result, but the continuation that carries it
        // is not in the window this runtime reads.
        const delivered = [...delegations.values()].some((task) =>
          DELIVERED.has(task.completionDelivery?.state ?? ""),
        );
        return {
          kind: "failed",
          explanation:
            delivered && historyOmitted
              ? OLDER_REPORT
              : "T3's delegated work ended without reporting back to this conversation",
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
            "A run T3's records cannot attribute followed this turn's, and T3's state cannot show whether it reports this turn's background work",
        };
      switch (last.status) {
        case "completed": {
          // A follow-up turn reports its continuations and never waits for a
          // wake: beside the conversation's later messages, no run but a
          // continuation can be attributed to it.
          if (followUp !== undefined) return reported(last);
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
                followUp: { message: messageId, after: anchor.ordinal },
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
      const partial = unconfirmed.size > 0 || workOmitted || owedUnknown;
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
        .map((item) => ({ ...tool(item), ...started(item) }));
      for (const id of firstSeen.keys())
        if (!running.has(id) && !held.has(id)) firstSeen.delete(id);
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
          estimated: true,
        });
      }
      for (const id of backgroundSince.keys())
        if (!tasks.some((task) => task.taskId === id)) backgroundSince.delete(id);
      const headline = latestAssistant === undefined ? undefined : commentary(latestAssistant.text);
      const work = workItems();
      const agent = agentOf(run);
      return {
        phase:
          run === undefined && followUp !== undefined
            ? "waiting"
            : phase(run, compacting.size > 0, retrying.size > 0, runningTools.length, held.size),
        ...(headline === undefined ? {} : { headline }),
        running: runningTools.slice(0, RUNNING_CALLS),
        recent: recent
          .filter((item) => !shownAsWork(item))
          .map(
            (item): FinishedTool => ({
              ...tool(item),
              ok: item.status === "completed",
              ...lasted(item.startedAt, item.completedAt),
            }),
          ),
        tool_calls: counted.size,
        ...(work.length === 0 && !partial ? {} : { work: summarizeWork(work, partial) }),
        ...(notes.size === 0 ? {} : { notes: [...notes.values()] }),
        ...(agent === undefined ? {} : { agent }),
      };
    },
    retained: () => ({
      runs: turnRuns.size + earlier.size,
      running: running.size + held.size,
      recent: recent.length,
      counted: counted.size,
      requests: requestItems.size + pendingRequests.size,
      messages: assistant.size,
      providerThreads: providerThreads.size,
      sessions: sessions.size,
      pendingProviderRecords: recentThreads.size + recentSessions.size + recentTransfers.size,
      detachedSessions: detachedSessions.size,
      delegations: delegations.size,
      transfers: transfers.size,
      links: ownLinks.size + pendingLinks.size,
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
  heldCalls: number,
): ActivityPhase {
  if (run?.status === "queued") return "queued";
  if (run === undefined || BEFORE_PROVIDER.has(run.status)) return "starting";
  if (compacting) return "compacting";
  if (retrying) return "retrying";
  if (runningTools > 0) return "tools";
  return run.status === "waiting" || heldCalls > 0 ? "waiting" : "model";
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

/** How long something ran, when T3 dated both its start and its end. */
function lasted(
  start: string | null | undefined,
  end: string | null | undefined,
): { duration_ms?: number } {
  const duration = Date.parse(end ?? "") - Date.parse(start ?? "");
  return Number.isNaN(duration) || duration < 0 ? {} : { duration_ms: duration };
}

function withDetail(detail: string | undefined): { detail?: string } {
  return detail === undefined ? {} : { detail };
}

/**
 * The run a message or notification item belongs to, and how that item would
 * open it; undefined for an item of no run or of another kind. Such an item
 * naming its run any other way cannot be passed over: its run would wait for
 * its opening.
 */
function openingOf(payload: unknown): readonly [string, Opening] | undefined {
  if (!isRecord(payload)) return undefined;
  const type = payload["type"];
  if (type !== "user_message" && type !== "notification") return undefined;
  const run = payload["runId"];
  if (run === null) return undefined;
  if (typeof run !== "string" || run.length === 0)
    throw new T3Failure("malformed", `T3 sent a ${type} item this runtime does not understand`);
  if (type === "user_message") return [run, "message"];
  if (Check(DelegatedNotificationSchema, payload)) return [run, "delegated"];
  if (Check(WakeNotificationSchema, payload)) return [run, "wake"];
  return [run, "unknown"];
}

/** When T3 made a message, as an instant: its time may be written with any offset. */
function madeAt(message: Message): number {
  return Date.parse(message.createdAt);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * A stream sequence, which T3 numbers with integers: a fraction would
 * become the replay cursor and skip the events below it.
 */
function isSequence(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value);
}
