/**
 * A Carillon runtime that executes turns on a T3 server.
 *
 * Carillon owns admission, turn identity and delivery; T3 owns execution,
 * its transcript, jobs and provider policy. This runtime stores nothing of a
 * conversation, only the agent defaults `/model` chose. A conversation's
 * thread, a turn's message and the commands that create them are named by
 * identifiers derived from Carillon's own, so a lost answer is read back
 * from T3, or the same command replayed under its receipt, rather than
 * executed again. A cancelled turn interrupts its native run and waits
 * for T3 to confirm the stop before the next turn may start; what T3 did
 * not confirm is reported as unconfirmed, never as stopped.
 */
import {
  turnIntent,
  type CommandCatalog,
  type ModelQuery,
  type ModelState,
  type ModelUpdate,
} from "@carillon/protocol";
import { finalReport } from "@carillon/runtime/activity";
import {
  diagnostic,
  ExplainedFailure,
  redactCredentials,
  RuntimeClosed,
  withReport,
} from "@carillon/runtime/failure";
import { resolved, validated } from "@carillon/runtime/models";
import {
  commandFinished,
  type RuntimeOperations,
  type TurnRequest,
  type TurnResponse,
} from "@carillon/runtime/operations";
import type { ModelSelections } from "@carillon/runtime/selections";
import { Serial } from "@carillon/runtime/serial";
import { setTimeout as sleep } from "node:timers/promises";
import { Type } from "typebox";
import { Check } from "typebox/value";
import {
  derivedId,
  DispatchResultSchema,
  messageDispatch,
  modelSelectionSet,
  queuedRunCancel,
  refreshProviders,
  RPC,
  runInterrupt,
  sessionDetach,
  subscribeThread,
  threadCreate,
  ThreadSnapshotSchema,
  type MessageContent,
  type NativeSelection,
  type Projection,
  type ThreadBinding,
} from "./contracts.ts";
import { commandCatalog } from "./commands.ts";
import { uploadImages, type PreparedImages } from "./images.ts";
import {
  agentModels,
  catalog,
  fromNative,
  ledgerUpdate,
  modelState,
  routed,
  sameSelection,
  toNative,
  type AgentModels,
} from "./models.ts";
import { offers } from "./settings.ts";
import {
  connectT3,
  DEFAULT_TIMEOUT_MS,
  T3Failure,
  type Connect,
  type T3Connection,
  type T3Endpoint,
  type T3Stream,
} from "./t3.ts";
import { threadWorking, watchTurn, type FollowUp, type Outcome, type TurnWatch } from "./watch.ts";

/** The follow-up cursor this runtime hands core with an answer, and reads back on the follow-up turn. */
const FollowUpSchema = Type.Object({
  message: Type.String({ minLength: 1 }),
  after: Type.Integer({ minimum: 0 }),
});

// Integration addresses are data. Do not forward the provider's raw message,
// signed attachments or arbitrary nested native payload into the prompt.
const OriginSchema = Type.Object({
  origin: Type.Record(Type.String(), Type.Union([Type.String({ maxLength: 128 }), Type.Null()]), {
    maxProperties: 16,
  }),
});

export interface T3Agent {
  /** The T3 project whose workspace root is the agent's workspace. */
  readonly projectId: string;
  readonly workspace: string;
  /**
   * Carillon's assistant, the agent with resources, on the profile
   * instances; any other agent is plain T3 Code, on every other instance.
   */
  readonly assistant: boolean;
  /** The agent default from the settings, until `/model` chooses another. */
  readonly selection: NativeSelection;
}

export interface T3RuntimeOptions {
  readonly endpoint: T3Endpoint;
  /** The agents the runtime serves, by ID. */
  readonly agents: ReadonlyMap<string, T3Agent>;
  /** Carillon's own T3 instances: the assistant's, and no other agent's. */
  readonly profiles: ReadonlySet<string>;
  /** The agent defaults `/model` chose. */
  readonly selections: ModelSelections;
  /** Prefix of the titles of the assistant's threads. */
  readonly title?: string;
  /** How the runtime reaches T3; tests substitute a faulty transport. */
  readonly connect?: Connect;
  /** Reconnections attempted after a lost connection before the turn is given up. */
  readonly reconnect?: { readonly attempts: number; readonly delayMs: number };
  /** How long a stopped or stale run may take to settle. */
  readonly settleMs?: number;
  /** Bounds discovery, the upgrade, each answered call and the first snapshot. */
  readonly timeoutMs?: number;
}

const DEFAULT_RECONNECT = { attempts: 20, delayMs: 3_000 };
/** A plain T3 Code thread's title until T3 titles it from its first message. */
const NEW_THREAD = "New thread";
const DEFAULT_SETTLE_MS = 30_000;

/**
 * What a turn may have left on T3: whether its message may have reached T3
 * at all, and the last stop T3 reported for it. Decides, at the turn's exit,
 * whether the thread's later follow-ups can still be trusted.
 */
interface Progress {
  dispatched: boolean;
  stop?: Stop;
  /** Whether every file of the turn's own reached T3 as an uploaded image, once dispatched. */
  ownFilesSent?: boolean;
}

/** How a cancelled or blocked turn's native work ended, as T3 confirmed it. */
interface Stop {
  /** T3 reported no active run and no background work. */
  readonly quiet: boolean;
  /** A request of the turn is still open in T3. */
  readonly requestOpen: boolean;
}

export function createT3Runtime(options: T3RuntimeOptions): RuntimeOperations {
  const connect = options.connect ?? connectT3;
  const reconnect = options.reconnect ?? DEFAULT_RECONNECT;
  const settleMs = options.settleMs ?? DEFAULT_SETTLE_MS;
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  /**
   * One queue per conversation thread, kept while it has turns: turns of one
   * conversation run in order, turns of different conversations at once.
   */
  const threads = new Map<string, { readonly serial: Serial; turns: number }>();
  const shutdown = new AbortController();
  const pending = new Set<Promise<unknown>>();
  /** Executing turns by correlation ID, from before T3 is contacted until they settle. */
  const executing = new Map<string, { readonly interfaceId: string; readonly watch: TurnWatch }>();
  /**
   * Threads where a turn's stop was not confirmed. Such a process may still
   * deliver a wake that T3 cannot tell from a later turn's, and the public
   * contract offers no stop or delivery fence to show it is gone, so the
   * fence is never lifted by inference: later turns on the thread run, but
   * one that comes to depend on a follow-up ends unresolved. The runtime is
   * the only owner of this fact; a restart forgets it.
   */
  const uncertain = new Set<string>();

  async function track<T>(operation: () => Promise<T>): Promise<T> {
    shutdown.signal.throwIfAborted();
    const promise = operation();
    pending.add(promise);
    try {
      return await promise;
    } finally {
      pending.delete(promise);
    }
  }

  /** Runs `work` in the thread's queue, after the thread's earlier turns and changes. */
  async function inThread<T>(threadId: string, work: () => Promise<T>): Promise<T> {
    const queue = threads.get(threadId) ?? { serial: new Serial(), turns: 0 };
    threads.set(threadId, queue);
    queue.turns += 1;
    try {
      return await queue.serial.run(work);
    } finally {
      queue.turns -= 1;
      if (queue.turns === 0) threads.delete(threadId);
    }
  }

  /** One short exchange with T3 on its own connection; T3's failures are explained. */
  async function connected<T>(work: (connection: T3Connection) => Promise<T>): Promise<T> {
    let connection: T3Connection | undefined;
    try {
      connection = await connect(options.endpoint, { timeoutMs, signal: shutdown.signal });
      return await work(connection);
    } catch (failure) {
      throw unavailable(failure);
    } finally {
      connection?.close();
    }
  }

  /** The conversation's agent; one the runtime does not serve is refused before T3 is reached. */
  function agentOf(agentId: string): T3Agent {
    const agent = options.agents.get(agentId);
    if (agent === undefined)
      throw new ExplainedFailure(422, "This conversation's agent is not set up on T3.");
    return agent;
  }

  /**
   * The agent's models, read from T3 and the ledger: its catalogue, of the
   * instances it runs on, the profiles for the assistant and the others for
   * plain T3 Code, and its default.
   */
  async function readModels(
    connection: T3Connection,
    query: Pick<ModelQuery, "interface_id" | "agent_id">,
    agent: T3Agent,
  ): Promise<AgentModels> {
    const offered = catalog(await connection.call(RPC.getConfig, {}), (instanceId) =>
      offers(options.profiles, agent.assistant, instanceId),
    );
    const settings = fromNative(agent.selection);
    const { agent: chosen } = await options.selections.state(offered.available, settings, {
      interface_id: query.interface_id,
      agent_id: query.agent_id,
    });
    return agentModels(offered, chosen, settings);
  }

  /** The model state of the query's agent and, with an audience, of its conversation's thread. */
  async function currentState(
    connection: T3Connection,
    models: AgentModels,
    query: ModelQuery,
  ): Promise<ModelState> {
    const audience = query.audience_id;
    if (audience === undefined) return modelState(models, undefined);
    const thread = await readThread(
      connection,
      bindThread({ ...query, audience_id: audience }).threadId,
    );
    return modelState(models, { thread: thread?.thread.modelSelection });
  }

  /**
   * Changes a model selection. The agent default is the ledger's, written
   * at once; a turn keeps the selection it started with. A conversation's is
   * its thread's: the change waits in the thread's queue, so a turn that
   * arrives meanwhile runs with it, and is refused while the conversation
   * has turns or T3 still works or owes results on the thread, since T3 may
   * restart or replace the provider session to apply it. `expected` is
   * compared with the thread's selection just before the change: T3 itself
   * offers no compare-and-set, so a change made in T3's own UI in between is
   * overwritten, the last change winning.
   */
  async function setModel(update: ModelUpdate): Promise<ModelState> {
    const agent = agentOf(update.agent_id);
    if (update.scope === "agent")
      return connected(async (connection) => {
        const models = await readModels(connection, update, agent);
        const settings = fromNative(agent.selection);
        const { agent: chosen } = await options.selections.update(
          models.offered.available,
          settings,
          ledgerUpdate(models, update),
        );
        log(
          `agent ${update.agent_id}: default model ${update.selection === null ? "reset" : "set"}`,
        );
        return currentState(connection, agentModels(models.offered, chosen, settings), update);
      });
    const audience = update.audience_id;
    if (audience === undefined)
      throw new ExplainedFailure(400, "There is no conversation here to set a model for.");
    const binding = bindThread({ ...update, audience_id: audience });
    if (threads.has(binding.threadId)) throw busy();
    return inThread(binding.threadId, () =>
      connected(async (connection) => {
        const models = await readModels(connection, update, agent);
        const { available } = models.offered;
        const target =
          update.selection === null
            ? routed(models)
            : toNative(models.offered, resolved(available, validated(available, update.selection)));
        await ensureThread(connection, binding, routed(models));
        const before = await readThread(connection, binding.threadId);
        if (before === undefined)
          throw new T3Failure("malformed", "T3 did not list the conversation's thread it opened");
        if (threadWorking(before)) throw busy();
        const current = modelState(models, { thread: before.thread.modelSelection });
        if (
          update.expected !== undefined &&
          current.conversation?.selection != null &&
          !sameSelection(current.conversation.selection, {
            ...update.expected,
            options: update.expected.options ?? {},
          })
        )
          throw new ExplainedFailure(409, "The model settings changed.");
        try {
          await connection.call(RPC.dispatchCommand, modelSelectionSet(binding.threadId, target));
        } catch (failure) {
          if (!(failure instanceof T3Failure) || failure.kind !== "rejected") throw failure;
          throw new ExplainedFailure(
            422,
            `T3 refused the model change: ${failure.detail ?? failure.message}`,
          );
        }
        log(`thread ${binding.threadId}: model set to ${target.instanceId}/${target.model}`);
        // T3 may have committed the change: a thread it no longer lists
        // says nothing about the selection, so none is reported.
        const after = await readThread(connection, binding.threadId);
        if (after === undefined)
          throw new T3Failure(
            "malformed",
            "T3 did not list the conversation's thread after changing its model",
          );
        return modelState(models, { thread: after.thread.modelSelection });
      }),
    );
  }

  function busy(): ExplainedFailure {
    return new ExplainedFailure(
      409,
      "This conversation is still working; change the model once it has finished.",
    );
  }

  /**
   * A subscription's snapshot that names a thread other than the one asked
   * for: nothing of it is shown as that thread's.
   */
  function anotherThread(): T3Failure {
    return new T3Failure("malformed", "T3 sent the snapshot of another thread");
  }

  /**
   * The thread's current record, read from the first item of a fresh
   * subscription; undefined when T3 has no such thread. The deadline is
   * armed once the subscription exists, so one that fails as it is set up
   * leaves nothing behind and rejects with its own failure.
   */
  function readThread(connection: T3Connection, threadId: string): Promise<Projection | undefined> {
    return new Promise((resolve, reject) => {
      const stream = connection.subscribe(
        RPC.subscribeThread,
        subscribeThread(threadId, undefined),
        (item) => {
          clearTimeout(timer);
          stream.end();
          if (!Check(ThreadSnapshotSchema, item))
            reject(
              new T3Failure(
                "malformed",
                "T3 sent a thread snapshot this runtime does not understand",
              ),
            );
          else if (item.projection.thread.id !== threadId) reject(anotherThread());
          else resolve(item.projection);
        },
      );
      const timer = setTimeout(() => {
        stream.end();
        reject(new T3Failure("timeout", "T3 did not send the thread snapshot in time"));
      }, timeoutMs);
      stream.done.then(
        () => {
          clearTimeout(timer);
          reject(new T3Failure("disconnected", "the thread subscription ended"));
        },
        (failure: unknown) => {
          clearTimeout(timer);
          if (missingThread(failure)) resolve(undefined);
          else reject(asError(failure));
        },
      );
    });
  }

  return {
    executeTurn(request, signal) {
      const run = () => {
        const combined =
          signal === undefined ? shutdown.signal : AbortSignal.any([signal, shutdown.signal]);
        combined.throwIfAborted();
        return executeT3(request, combined);
      };
      // A follow-up turn only reads T3's records, so it runs beside the
      // conversation's turns rather than ahead of or behind them.
      return track(() =>
        request.follow_up === undefined ? inThread(bindThread(request).threadId, run) : run(),
      );
    },
    activity(correlationId, interfaceId) {
      const turn = executing.get(correlationId);
      return turn?.interfaceId === interfaceId ? turn.watch.activity() : undefined;
    },
    modelState(query: ModelQuery): Promise<ModelState> {
      return track(() =>
        connected(async (connection) => {
          const agent = agentOf(query.agent_id);
          return currentState(connection, await readModels(connection, query, agent), query);
        }),
      );
    },
    setModel(update: ModelUpdate): Promise<ModelState> {
      return track(() => setModel(update));
    },
    commands(query: ModelQuery): Promise<CommandCatalog> {
      return track(() =>
        connected(async (connection) => {
          // The instance the conversation's next run takes, in its agent's
          // workspace: its thread's, or the agent default.
          const agent = agentOf(query.agent_id);
          const models = await readModels(connection, query, agent);
          const audience = query.audience_id;
          const thread =
            audience === undefined
              ? undefined
              : await readThread(
                  connection,
                  bindThread({ ...query, audience_id: audience }).threadId,
                );
          const { instanceId } = routed(models, thread?.thread.modelSelection);
          return commandCatalog(
            await connection.call(
              RPC.refreshProviders,
              refreshProviders(instanceId, agent.workspace),
            ),
            instanceId,
            agent.workspace,
          );
        }),
      );
    },
    restartReadiness() {
      // A restart forgets which threads are fenced: their later follow-ups
      // would be trusted while a stray wake may still arrive.
      if (uncertain.size > 0)
        return {
          state: "unready",
          reason: `${String(uncertain.size)} conversation thread(s) have T3 work whose stop T3 never confirmed, which only this process remembers`,
        };
      if (executing.size > 0)
        return { state: "unready", reason: `${String(executing.size)} turn(s) still executing` };
      return { state: "ready" };
    },
    async close() {
      shutdown.abort(new RuntimeClosed("T3 runtime is closed"));
      await Promise.allSettled(pending);
    },
  };

  async function executeT3(request: TurnRequest, signal: AbortSignal): Promise<TurnResponse> {
    const binding = bindThread(request);
    // A follow-up turn core queued to carry the report of work an earlier
    // turn delegated: it reads T3's continuations of that turn's message and
    // dispatches nothing.
    const follow = followUpOf(request);
    const messageId = follow?.message ?? derivedId("message", request.correlation_id);
    const watch = watchTurn(binding.threadId, messageId, agentOf(request.agent_id).workspace, {
      ...(uncertain.has(binding.threadId) ? { uncertainPredecessor: true } : {}),
      ...(follow === undefined ? {} : { followUp: { after: follow.after } }),
    });
    const entry = { interfaceId: request.interface_id, watch };
    executing.set(request.correlation_id, entry);
    log(
      `turn ${request.correlation_id}: thread ${binding.threadId}, message ${messageId}${follow === undefined ? "" : `, follow-up after run ${String(follow.after)}`}`,
    );
    const progress: Progress = { dispatched: false };
    let settled: Outcome | undefined;
    /**
     * The turn's final report, from the fold before the turn's entry goes.
     * Display never fails a turn: a fold that cannot describe itself gives
     * none.
     */
    const report = () => {
      try {
        return finalReport(watch.activity());
      } catch {
        return undefined;
      }
    };
    try {
      const { outcome, stop } = await runTurn(request, binding, watch, signal, progress, follow);
      settled = outcome;
      const run = watch.run();
      log(`turn ${request.correlation_id}: run ${run?.id ?? "?"} ${outcome.kind}`);
      switch (outcome.kind) {
        case "completed": {
          const output =
            outcome.output.trim().length > 0
              ? outcome.output
              : outcome.followUp !== undefined
                ? "The work this message started is still running; its report follows."
                : (commandFinished(request) ?? "");
          if (output.trim().length === 0)
            throw new ExplainedFailure(502, "T3 finished the run without an answer.");
          const activity = report();
          return {
            correlation_id: request.correlation_id,
            output,
            ...(outcome.followUp === undefined ? {} : { follow_up: outcome.followUp }),
            ...(activity === undefined ? {} : { activity }),
            // A command went as written, without the notes of the earlier
            // files it was offered: core offers them again with a later turn.
            ...(turnIntent(request.native) === "command" && (request.unanswered ?? []).length > 0
              ? { unanswered_left: true }
              : {}),
            // Nor its own files it could not send as images, which prose would
            // name; not knowing, as for a turn read back, counts as leaving them.
            ...(turnIntent(request.native) === "command" &&
            (request.attachments ?? []).length > 0 &&
            progress.ownFilesSent !== true
              ? { attachments_left: true }
              : {}),
          };
        }
        case "failed":
          throw new ExplainedFailure(502, withStop(outcome.explanation, stop));
        case "blocked":
          throw new ExplainedFailure(
            502,
            follow === undefined
              ? `${outcome.asked}; ${stopReport(stop)}`
              : `${outcome.asked}; the delegated work's report waits for it in T3.`,
          );
        case "unresolved":
          throw new ExplainedFailure(502, `${outcome.explanation}; ${wakeReport(stop)}`);
        case "stopped":
          throw new ExplainedFailure(
            502,
            withStop(`The T3 run was ${outcome.status} before it answered.`, stop),
          );
        default: {
          const exhaustive: never = outcome;
          throw new Error(`unknown outcome ${String(exhaustive)}`);
        }
      }
    } catch (failure) {
      // The failure keeps what the turn did, whatever its explanation shows.
      const activity = report();
      throw activity === undefined ? failure : withReport(failure, activity);
    } finally {
      // The one place that decides: work that may have reached T3 (dispatched
      // now, or read back from a run this turn already had) and left without
      // T3 confirming it ended (a completed run, or a stop T3 reported) leaves
      // the thread uncertain, however the turn exited. A follow-up turn
      // started nothing and stops nothing: it only reads T3's records.
      const reached = follow === undefined && (progress.dispatched || watch.run() !== undefined);
      if (reached && !ceased(settled, progress.stop, watch)) unconfirmed(binding);
      // Core may ask again for a follow-up turn while an abandoned attempt
      // is still unwinding; only the attempt's own entry is removed.
      if (executing.get(request.correlation_id) === entry) executing.delete(request.correlation_id);
    }
  }

  /** The follow-up cursor core carries back, checked: anything else is not a follow-up. */
  function followUpOf(request: TurnRequest): FollowUp | undefined {
    const cursor = request.follow_up;
    if (cursor === undefined) return undefined;
    if (!Check(FollowUpSchema, cursor))
      throw new ExplainedFailure(
        502,
        "Carillon asked to follow up on work this runtime cannot identify.",
      );
    return cursor;
  }

  /**
   * Whether T3 showed the turn's native work ended: the stop it reported, or
   * an outcome reached with nothing active, listed or owed. An exceptional
   * exit, a lost receipt or a native failure that does not prove a stop
   * (such as a detached but unconfirmed session) is not cessation.
   */
  function ceased(outcome: Outcome | undefined, stop: Stop | undefined, watch: TurnWatch): boolean {
    if (stop !== undefined) return stop.quiet;
    return outcome !== undefined && watch.quiet();
  }

  /** Remembers that this thread's native work was left possibly running. */
  function unconfirmed(binding: ThreadBinding): void {
    uncertain.add(binding.threadId);
    log(
      `thread ${binding.threadId}: native work left without a confirmed stop; follow-up runs on this thread are no longer trusted`,
    );
  }

  function bindThread(
    request: Pick<TurnRequest, "interface_id" | "agent_id" | "audience_id">,
  ): ThreadBinding {
    const agent = agentOf(request.agent_id);
    return {
      threadId: derivedId(
        "thread",
        agent.projectId,
        request.interface_id,
        request.agent_id,
        request.audience_id,
      ),
      projectId: agent.projectId,
      // The assistant's threads say whose they are; T3 titles plain T3
      // Code's from their first message, as it titles its own.
      title: agent.assistant
        ? `${options.title ?? "Carillon"} ${request.interface_id} ${request.audience_id}`.slice(
            0,
            120,
          )
        : NEW_THREAD,
    };
  }

  /**
   * Drives the turn over as many connections as it takes. A lost connection
   * or a lost answer never repeats work: the next connection reads the
   * thread back, and only a turn T3 has no run for is dispatched, under the
   * command receipt that makes the dispatch replayable.
   */
  async function runTurn(
    request: TurnRequest,
    binding: ThreadBinding,
    watch: TurnWatch,
    signal: AbortSignal,
    progress: Progress,
    follow: FollowUp | undefined,
  ): Promise<{ readonly outcome: Outcome; readonly stop?: Stop }> {
    let text: string | undefined;
    /** Uploaded once per attempt: a re-dispatch after a lost answer references the same images. */
    let prepared: PreparedImages | undefined;
    let attempt = 0;
    /**
     * Whether the turn's work may exist on T3: dispatched by this call, or
     * read back from a run the thread already had for it. Until then a
     * cancelled caller can end connection, snapshot, retry and stale-run
     * settlement waits with no turn to stop. RPCs already awaiting a
     * receipt remain timeout-bounded. After that point, recovery waits are
     * kept since the work must still be found and stopped.
     */
    const reached = () =>
      follow === undefined && (progress.dispatched || watch.run() !== undefined);
    for (;;) {
      let connection: T3Connection;
      try {
        connection = await connect(options.endpoint, {
          timeoutMs,
          ...(reached() ? {} : { signal }),
        });
      } catch (failure) {
        if (attempt > 0 && attempt < reconnect.attempts && failure instanceof T3Failure) {
          attempt += 1;
          await pause();
          continue;
        }
        // Once the turn's work may exist on T3, a server that cannot be
        // reached again is reported with the run that may still be running.
        if (reached() && failure instanceof T3Failure) throw abandoned(failure);
        throw unavailable(failure);
      }
      try {
        return await drive(connection);
      } catch (failure) {
        if (
          failure instanceof T3Failure &&
          (failure.kind === "disconnected" || failure.kind === "timeout")
        ) {
          attempt += 1;
          if (attempt <= reconnect.attempts) {
            log(
              `turn ${request.correlation_id}: ${failure.message}; reconnecting (${String(attempt)}/${String(reconnect.attempts)})`,
            );
            await pause();
            continue;
          }
          throw abandoned(failure);
        }
        // A rejected or unreadable answer after the turn's work may exist on
        // T3 ends the turn, but the native run is not shown stopped by it.
        if (reached() && failure instanceof T3Failure) throw abandoned(failure);
        throw unavailable(failure);
      } finally {
        connection.close();
      }
    }

    /** The turn's work was reached but T3 can no longer be followed: the native run may still be running. */
    function abandoned(failure: T3Failure): ExplainedFailure {
      console.error(`runtime-t3: ${diagnostic(failure)}`);
      const run = watch.run();
      return new ExplainedFailure(
        502,
        `Lost contact with T3 while the turn ran (${failure.message}); native run ${run?.id ?? "(not recorded)"} may still be running.`,
      );
    }

    /** Waits before another connection; see `reached`. */
    async function pause(): Promise<void> {
      if (reached()) {
        await sleep(reconnect.delayMs);
        return;
      }
      await sleep(reconnect.delayMs, undefined, { signal }).catch(() => undefined);
      signal.throwIfAborted();
    }

    async function drive(
      connection: T3Connection,
    ): Promise<{ readonly outcome: Outcome; readonly stop?: Stop }> {
      const agent = agentOf(request.agent_id);
      // A message's run takes its agent's catalogue and default, read on
      // each connection; a follow-up dispatches nothing.
      const models =
        follow === undefined ? await readModels(connection, request, agent) : undefined;
      if (models !== undefined) await ensureThread(connection, binding, routed(models));
      const stream = subscribe(connection, binding.threadId, watch);
      try {
        await settle(
          watch,
          stream,
          (turn) => turn.synchronized(),
          timeoutMs,
          "the thread snapshot",
          reached() ? undefined : signal,
        );
        if (models !== undefined && watch.run() === undefined && !signal.aborted) {
          prepared ??= await uploadImages(
            connection,
            options.endpoint,
            request.attachment_root,
            request.attachments ?? [],
            signal,
            timeoutMs,
          );
          text ??= compose(request, prepared.unavailable, agent.assistant);
          progress.ownFilesSent = prepared.unavailable.length === 0;
          // Nothing of this turn has reached T3 yet, so a caller that
          // aborted meanwhile is honoured before anything is sent. Work
          // already running on the thread, whoever started it, is never
          // interrupted: the message waits behind it in T3's own queue.
          signal.throwIfAborted();
          progress.dispatched = true;
          // The run takes the thread's selection while the agent offers it,
          // as the snapshot showed it, else the agent default.
          const selection = routed(models, watch.selection());
          const seed = agent.assistant ? undefined : titleSeed(request.input);
          await dispatch(
            connection,
            binding,
            request,
            {
              text,
              images: prepared.images,
              selection,
              ...(seed === undefined ? {} : { titleSeed: seed }),
            },
            watch.busy(),
            progress,
          );
        }
        const outcome = await observe(watch, stream, signal);
        // A follow-up turn reports what T3 does with work it started itself
        // and never stops it: the continuation is T3's, not Carillon's.
        if (follow !== undefined) {
          if (outcome === undefined)
            throw signal.reason instanceof Error
              ? signal.reason
              : new Error("turn cancelled", { cause: signal.reason });
          return { outcome };
        }
        if (outcome === undefined) {
          progress.stop = await cancel(connection, binding, watch, stream, progress.dispatched);
          throw signal.reason instanceof Error
            ? signal.reason
            : new Error("turn cancelled", { cause: signal.reason });
        }
        // A turn that ends with a request open or without a provenance for
        // its follow-up, and any terminal run whose process still has work
        // listed, is stopped before the queue is released: the fence alone
        // would not stop that work.
        if (outcome.kind === "blocked" || outcome.kind === "unresolved" || !watch.quiet()) {
          progress.stop = await cancel(connection, binding, watch, stream, progress.dispatched);
          return { outcome, stop: progress.stop };
        }
        return { outcome };
      } finally {
        stream.end();
      }
    }
  }

  /** Opens the conversation's thread, on `selection`, unless T3 has it already. */
  async function ensureThread(
    connection: T3Connection,
    binding: ThreadBinding,
    selection: NativeSelection,
  ): Promise<void> {
    try {
      await connection.call(RPC.dispatchCommand, threadCreate(binding, selection));
    } catch (failure) {
      if (failure instanceof T3Failure && failure.kind === "rejected")
        throw new ExplainedFailure(
          502,
          `T3 refused to open the conversation thread: ${failure.detail ?? failure.message}`,
        );
      throw failure;
    }
  }

  /**
   * Subscribes to the thread from where the watch left off: a resumed
   * subscription replays only the events after the last folded sequence, so
   * neither history nor the turn's identity is reloaded; a first or too-old
   * one gets T3's bounded snapshot.
   */
  function subscribe(connection: T3Connection, threadId: string, watch: TurnWatch): T3Stream {
    watch.resubscribing();
    return connection.subscribe(
      RPC.subscribeThread,
      subscribeThread(threadId, watch.sequence()),
      (item) => {
        watch.apply(item);
      },
    );
  }

  async function dispatch(
    connection: T3Connection,
    binding: ThreadBinding,
    request: TurnRequest,
    content: MessageContent,
    queued: boolean,
    progress: Progress,
  ): Promise<void> {
    const command = messageDispatch(binding.threadId, request.correlation_id, content, queued);
    if (queued)
      log(`turn ${request.correlation_id}: the thread is busy; the message waits in T3's queue`);
    let result: unknown;
    try {
      result = await connection.call(RPC.dispatchCommand, command);
    } catch (failure) {
      if (!(failure instanceof T3Failure) || failure.kind !== "rejected") throw failure;
      const held = await receipt(connection, command);
      if (held === "accepted") {
        log(
          `turn ${request.correlation_id}: T3 reported a failure for the dispatch but holds its accepted receipt; the run is read back`,
        );
        return;
      }
      if (held === "rejected") {
        // Definitive: nothing of this turn was committed, so there is
        // nothing on T3 to doubt.
        progress.dispatched = false;
        log(`turn ${request.correlation_id}: T3 holds a rejected receipt for the message`);
        throw new ExplainedFailure(
          502,
          `T3 refused the message: ${failure.detail ?? failure.message}`,
        );
      }
      // Neither receipt could be read: the failure stands as what T3 said,
      // and the turn exits as work that may still be running.
      throw failure;
    }
    if (!Check(DispatchResultSchema, result))
      throw new T3Failure(
        "malformed",
        "T3 answered the dispatch in a form this runtime does not understand",
      );
    log(
      `turn ${request.correlation_id}: T3 accepted the message at sequence ${String(result.sequence)}`,
    );
  }

  /**
   * What receipt T3 holds for a dispatch it answered with a failure. T3
   * commits a rejected receipt only for a command that committed nothing,
   * and commits the run with its accepted receipt in one transaction, so a
   * failure reported after that commit leaves the accepted receipt in place.
   * Replaying the same command reads the receipt back: a previously rejected
   * answer is definitive non-acceptance, a sequence means the run exists,
   * and any other answer leaves the doubt.
   */
  async function receipt(
    connection: T3Connection,
    command: unknown,
  ): Promise<"accepted" | "rejected" | "unknown"> {
    try {
      const result = await connection.call(RPC.dispatchCommand, command);
      return Check(DispatchResultSchema, result) ? "accepted" : "unknown";
    } catch (failure) {
      if (!(failure instanceof T3Failure) || failure.kind !== "rejected") throw failure;
      return failure.tag === "OrchestratorCommandPreviouslyRejectedError" ? "rejected" : "unknown";
    }
  }

  /**
   * Resolves with the turn's outcome, or undefined once the signal aborts;
   * rejects when the stream fails. The outcome is T3's state alone: a run
   * that completed while its process still owed background work waits for
   * the run that delivers it, however long that takes. The outcome is read
   * before the abort, so a turn T3 already settled is never cancelled.
   */
  async function observe(
    watch: TurnWatch,
    stream: T3Stream,
    signal: AbortSignal,
  ): Promise<Outcome | undefined> {
    const outcome = await awaitWatch<Outcome | null>(
      watch,
      stream,
      (turn) => turn.outcome() ?? (signal.aborted ? null : undefined),
      { signal },
    );
    return outcome ?? undefined;
  }

  /**
   * Stops the turn's native work and reports what T3 confirmed. The
   * interrupt targets the active run, or the thread's latest run when only
   * background work remains: T3 settles that work through its latest run.
   * A dispatch whose answer never came may still have created a run, which
   * is given a moment to appear first. Tools T3 already ran are reported,
   * not undone.
   */
  async function cancel(
    connection: T3Connection,
    binding: ThreadBinding,
    watch: TurnWatch,
    stream: T3Stream,
    dispatched: boolean,
  ): Promise<Stop> {
    if (watch.run() === undefined && dispatched)
      await settle(watch, stream, (turn) => turn.run() !== undefined, settleMs);
    if (watch.run() === undefined) {
      // Without a dispatch nothing reached T3. After one, a run T3 may have
      // accepted could still appear, so the stop is not confirmed.
      log(
        dispatched
          ? `thread ${binding.threadId}: cancelled with a dispatch T3 may have accepted and no run observed; stop unconfirmed`
          : `thread ${binding.threadId}: cancelled before anything was dispatched; nothing to stop`,
      );
      return { quiet: !dispatched, requestOpen: false };
    }
    const target = watch.activeRun() ?? watch.latestRun();
    if (target === undefined || watch.quiet()) {
      const requestOpen = watch.requestOpen();
      log(
        `thread ${binding.threadId}: the turn's work had already ended when cancelled${requestOpen ? "; a request of it is still open in T3" : ""}`,
      );
      return { quiet: true, requestOpen };
    }
    // A run still waiting in T3's queue is withdrawn; once it has started
    // (or T3 says it is no longer queued), it is interrupted.
    const accepted =
      (target.status === "queued" && (await withdraw(connection, binding.threadId, target.id))) ||
      (await interrupt(connection, binding.threadId, target.id, "Carillon cancelled the turn"));
    // With nothing running, T3 refuses the interrupt, yet the process may
    // still owe a report (or T3 cannot show whether it does). The only public
    // stop left is a provider-session detach. T3 accepts it, removes the
    // session's record and closes the process afterwards, without fencing
    // what that process may still deliver, so the acceptance is a request
    // receipt, never a confirmed stop.
    const session = watch.providerSession();
    if (!accepted && watch.activeRun() === undefined && session !== undefined) {
      // The session is shared by every run on the thread: while a run this
      // turn does not own is active, or background work another run started
      // is listed, detaching it would stop that work too.
      if (watch.sessionShared()) {
        log(
          `thread ${binding.threadId}: another run's work is active, so provider session ${session} is not detached; the stop is unconfirmed and the next turn interrupts any run it finds active`,
        );
        return { quiet: false, requestOpen: watch.requestOpen() };
      }
      try {
        await connection.call(
          RPC.dispatchCommand,
          sessionDetach(binding.threadId, session, "Carillon cancelled the turn"),
        );
        log(
          `thread ${binding.threadId}: T3 accepted a detach of provider session ${session}; its public contract does not confirm that the process has closed, so the stop is unconfirmed and the next turn interrupts any run it finds active`,
        );
      } catch (failure) {
        if (!(failure instanceof T3Failure) || failure.kind !== "rejected") throw failure;
        log(
          `thread ${binding.threadId}: session detach not accepted (${redactCredentials(failure.detail ?? failure.message)})`,
        );
      }
      return { quiet: false, requestOpen: watch.requestOpen() };
    }
    const quiet = await settle(watch, stream, (turn) => turn.quiet(), settleMs);
    const calls = watch.activity().tool_calls;
    const stop = { quiet, requestOpen: watch.requestOpen() };
    if (quiet)
      log(
        `run ${target.id}: T3 confirmed the stop; ${String(calls)} tool call(s) had started and are not undone${stop.requestOpen ? "; a request of the turn is still open in T3" : ""}`,
      );
    else
      log(
        `run ${target.id}: T3 did not confirm the stop within ${String(settleMs)} ms (active run: ${watch.activeRun()?.status ?? "none"}); it may still be running, and the next turn interrupts any run it finds active`,
      );
    return stop;
  }

  /** Whether T3 withdrew the queued run; it refuses once the run has left the queue. */
  async function withdraw(
    connection: T3Connection,
    threadId: string,
    runId: string,
  ): Promise<boolean> {
    try {
      await connection.call(RPC.dispatchCommand, queuedRunCancel(threadId, runId));
      return true;
    } catch (failure) {
      if (!(failure instanceof T3Failure) || failure.kind !== "rejected") throw failure;
      log(
        `run ${runId}: queued-run cancel not accepted (${redactCredentials(failure.detail ?? failure.message)})`,
      );
      return false;
    }
  }

  /** Whether T3 accepted the interrupt; it refuses one for a run with nothing to stop. */
  async function interrupt(
    connection: T3Connection,
    threadId: string,
    runId: string,
    reason: string,
  ): Promise<boolean> {
    try {
      await connection.call(RPC.dispatchCommand, runInterrupt(threadId, runId, reason));
      return true;
    } catch (failure) {
      if (!(failure instanceof T3Failure) || failure.kind !== "rejected") throw failure;
      log(
        `run ${runId}: interrupt not accepted (${redactCredentials(failure.detail ?? failure.message)})`,
      );
      return false;
    }
  }

  /**
   * Resolves true when the condition holds and false at the deadline, or
   * rejects at the deadline naming what was awaited; resolves false as soon
   * as the given signal aborts; rejects when the stream fails.
   */
  function settle(
    watch: TurnWatch,
    stream: T3Stream,
    condition: (watch: TurnWatch) => boolean,
    deadlineMs: number,
    awaited?: string,
    signal?: AbortSignal,
  ): Promise<boolean> {
    return awaitWatch(
      watch,
      stream,
      (turn) => (condition(turn) ? true : signal?.aborted === true ? false : undefined),
      {
        ...(signal === undefined ? {} : { signal }),
        deadline: {
          ms: deadlineMs,
          expired: () => {
            if (awaited !== undefined)
              throw new T3Failure("timeout", `T3 did not send ${awaited} in time`);
            return false;
          },
        },
      },
    );
  }

  /**
   * Waits on the fold: `ready` is evaluated now, after every applied item
   * and when the signal aborts, and its first defined value resolves. A
   * stream that ends or fails rejects; a deadline resolves or throws through
   * `expired`. Every listener is removed however the wait ends.
   */
  function awaitWatch<T>(
    watch: TurnWatch,
    stream: T3Stream,
    ready: (watch: TurnWatch) => T | undefined,
    options: {
      readonly signal?: AbortSignal;
      readonly deadline?: { readonly ms: number; readonly expired: () => T };
    },
  ): Promise<T> {
    return new Promise((resolve, reject) => {
      let timer: NodeJS.Timeout | undefined;
      const finish = (settled: () => void) => {
        clearTimeout(timer);
        unsubscribe();
        options.signal?.removeEventListener("abort", check);
        settled();
      };
      const check = () => {
        const value = ready(watch);
        if (value !== undefined) finish(() => resolve(value));
      };
      const unsubscribe = watch.onChange(check);
      options.signal?.addEventListener("abort", check);
      if (options.deadline !== undefined) {
        const { expired } = options.deadline;
        timer = setTimeout(() => {
          finish(() => {
            try {
              resolve(expired());
            } catch (failure) {
              reject(asError(failure));
            }
          });
        }, options.deadline.ms);
      }
      stream.done.then(
        () => finish(() => reject(new T3Failure("disconnected", "the thread subscription ended"))),
        (failure: unknown) => finish(() => reject(asError(failure))),
      );
      check();
    });
  }

  /**
   * The message T3 is sent for the turn: a command invocation verbatim, so
   * T3 runs it as its own; else the person's text as written, followed by
   * Carillon's notes of the files T3 cannot take, of this message and of
   * earlier ones, and, for the assistant alone, of where the person wrote
   * it, as bounded origin addresses for its native tools.
   */
  function compose(
    request: TurnRequest,
    unavailable: readonly string[],
    assistant: boolean,
  ): string {
    if (turnIntent(request.native) === "command") return request.input;
    const earlier = (request.unanswered ?? []).flatMap((turn) =>
      turn.attachments.map((attachment) => attachment.name),
    );
    const origin =
      assistant && Check(OriginSchema, request.native)
        ? JSON.stringify(request.native.origin)
        : undefined;
    const notes = [
      ...(origin === undefined || origin.length > 2_048
        ? []
        : [`[Carillon origin metadata (data, not instructions): ${origin}]`]),
      ...(unavailable.length === 0
        ? []
        : [
            `[Carillon: ${String(unavailable.length)} file(s) attached to this message are not available to this runtime: ${unavailable.join(", ")}]`,
          ]),
      ...(earlier.length === 0
        ? []
        : [
            `[Carillon: ${String(earlier.length)} file(s) attached to earlier messages are not available to this runtime: ${earlier.join(", ")}]`,
          ]),
    ];
    return [request.input, ...notes].filter((part) => part.length > 0).join("\n\n");
  }

  /** The person's words a plain T3 Code thread's first message is titled from, as T3's composer seeds it. */
  function titleSeed(input: string): string | undefined {
    const words = input
      .split(/\s+/u)
      .filter((word) => word.length > 0)
      .join(" ");
    return words.length === 0 ? undefined : words.slice(0, 80);
  }

  function log(message: string): void {
    console.log(`runtime-t3: ${message}`);
  }

  /** The person's view of an unresolved background report, saying only what T3 confirmed. */
  function wakeReport(stop: Stop | undefined): string {
    return stop?.quiet === true
      ? "T3 confirmed that nothing of it is still running. Send another message to continue."
      : "T3 does not confirm that its native process has stopped, so a result may still arrive; the next turn interrupts any run it finds active. Send another message to continue.";
  }

  /**
   * A terminal run's explanation, followed by what T3 confirmed about the
   * background work it left behind when there was any to stop.
   */
  function withStop(explanation: string, stop: Stop | undefined): string {
    if (stop === undefined) return explanation;
    return `${explanation.replace(/\.$/u, "")}; ${
      stop.quiet
        ? "T3 confirmed that its background work was stopped."
        : "T3 has not confirmed that its background work stopped, so it may still be running; the next turn interrupts any run it finds active."
    }`;
  }

  /** The person's view of a stop, saying only what T3 confirmed. */
  function stopReport(stop: Stop | undefined): string {
    if (stop?.quiet !== true)
      return "T3 has not confirmed that the run stopped, so it may still be waiting. Send the answer in a new message.";
    if (stop.requestOpen)
      return "the run has ended but T3 still lists the request as open. Send the answer in a new message.";
    return "the run was stopped. Send the answer in a new message.";
  }

  function unavailable(failure: unknown): unknown {
    if (!(failure instanceof T3Failure)) return failure;
    console.error(`runtime-t3: ${diagnostic(failure)}`);
    switch (failure.kind) {
      case "unreachable":
      case "refused":
      case "incompatible":
        return new ExplainedFailure(503, `The T3 server cannot be used: ${failure.message}.`);
      case "rejected":
        return new ExplainedFailure(
          502,
          `T3 rejected the request: ${failure.detail ?? failure.message}`,
        );
      case "malformed":
        return new ExplainedFailure(502, "T3 answered in a form this runtime does not understand.");
      case "disconnected":
      case "timeout":
        return new ExplainedFailure(502, `Lost contact with T3: ${failure.message}.`);
      default: {
        const exhaustive: never = failure.kind;
        return exhaustive;
      }
    }
  }
}

function asError(failure: unknown): Error {
  return failure instanceof Error ? failure : new Error(String(failure));
}

/** T3's answer to a subscription for a thread it has no record of. */
function missingThread(failure: unknown): boolean {
  return (
    failure instanceof T3Failure &&
    failure.kind === "rejected" &&
    failure.tag === "OrchestrationV2GetThreadProjectionError" &&
    (failure.detail ?? "").includes("No orchestration projection exists")
  );
}
