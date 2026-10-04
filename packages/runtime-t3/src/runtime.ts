/**
 * A Carillon runtime that executes turns on a T3 server.
 *
 * Carillon owns admission, turn identity and delivery; T3 owns execution,
 * its transcript, jobs and provider policy. This runtime stores nothing: a
 * conversation's thread, a turn's message and the commands that create them
 * are named by identifiers derived from Carillon's own, so a lost answer is
 * read back from T3, or the same command replayed under its receipt, rather
 * than executed again. A cancelled turn interrupts its native run and waits
 * for T3 to confirm the stop before the next turn may start; what T3 did
 * not confirm is reported as unconfirmed, never as stopped.
 */
import type { ModelQuery, ModelState } from "@carillon/protocol";
import { diagnostic, ExplainedFailure } from "@carillon/runtime-pi/failure";
import type { RuntimeOperations, TurnRequest, TurnResponse } from "@carillon/runtime-pi/operations";
import { Serial } from "@carillon/runtime-pi/serial";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { Check } from "typebox/value";
import {
  derivedId,
  DispatchResultSchema,
  messageDispatch,
  RPC,
  runInterrupt,
  sessionDetach,
  subscribeThread,
  threadCreate,
  type NativeSelection,
  type ThreadBinding,
} from "./contracts.ts";
import {
  connectT3,
  DEFAULT_TIMEOUT_MS,
  T3Failure,
  type Connect,
  type T3Connection,
  type T3Endpoint,
  type T3Stream,
} from "./t3.ts";
import { watchTurn, type Outcome, type TurnWatch } from "./watch.ts";

export interface T3RuntimeOptions {
  readonly endpoint: T3Endpoint;
  /** The T3 project whose workspace root is the agent's workspace. */
  readonly projectId: string;
  readonly selection: NativeSelection;
  readonly workspace: string;
  /** Prefix of the thread titles this runtime opens. */
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
const DEFAULT_SETTLE_MS = 30_000;

/**
 * What a turn may have left on T3: whether its message may have reached T3
 * at all, and the last stop T3 reported for it. Decides, at the turn's exit,
 * whether the thread's later follow-ups can still be trusted.
 */
interface Progress {
  dispatched: boolean;
  stop?: Stop;
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
  const turns = new Serial();
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

  return {
    executeTurn(request, signal) {
      return track(() =>
        turns.run(() => {
          const combined =
            signal === undefined ? shutdown.signal : AbortSignal.any([signal, shutdown.signal]);
          combined.throwIfAborted();
          return executeT3(request, combined);
        }),
      );
    },
    activity(correlationId, interfaceId) {
      const turn = executing.get(correlationId);
      return turn?.interfaceId === interfaceId ? turn.watch.activity() : undefined;
    },
    modelState(query: ModelQuery): Promise<ModelState> {
      shutdown.signal.throwIfAborted();
      return Promise.resolve(fixedModelState(options.selection, query));
    },
    setModel(): Promise<ModelState> {
      shutdown.signal.throwIfAborted();
      return Promise.reject(
        new ExplainedFailure(
          422,
          "The T3 runtime runs the model named in its settings; change the provider file and restart to select another.",
        ),
      );
    },
    async close() {
      shutdown.abort(new Error("T3 runtime is closed"));
      await Promise.allSettled([...pending]);
    },
  };

  async function executeT3(request: TurnRequest, signal: AbortSignal): Promise<TurnResponse> {
    if ((request.attachments ?? []).length > 0)
      throw new ExplainedFailure(
        422,
        "This runtime cannot accept attached files yet; please send text only.",
      );
    const binding = bindThread(request);
    const messageId = derivedId("message", request.correlation_id);
    const watch = watchTurn(
      messageId,
      options.workspace,
      uncertain.has(binding.threadId) ? { uncertainPredecessor: true } : {},
    );
    executing.set(request.correlation_id, { interfaceId: request.interface_id, watch });
    log(`turn ${request.correlation_id}: thread ${binding.threadId}, message ${messageId}`);
    const progress: Progress = { dispatched: false };
    let settled: Outcome | undefined;
    try {
      const { outcome, stop } = await runTurn(request, binding, watch, signal, progress);
      settled = outcome;
      const run = watch.run();
      log(`turn ${request.correlation_id}: run ${run?.id ?? "?"} ${outcome.kind}`);
      switch (outcome.kind) {
        case "completed":
          if (outcome.output.trim().length === 0)
            throw new ExplainedFailure(502, "T3 finished the run without an answer.");
          return { correlation_id: request.correlation_id, output: outcome.output };
        case "failed":
          throw new ExplainedFailure(502, outcome.explanation);
        case "blocked":
          throw new ExplainedFailure(502, `${outcome.asked}; ${stopReport(stop)}`);
        case "unresolved":
          throw new ExplainedFailure(502, `${outcome.explanation}; ${wakeReport(stop)}`);
        case "stopped":
          throw new ExplainedFailure(502, `The T3 run was ${outcome.status} before it answered.`);
        default: {
          const exhaustive: never = outcome;
          throw new Error(`unknown outcome ${String(exhaustive)}`);
        }
      }
    } finally {
      // The one place that decides: work that may have reached T3 (dispatched
      // now, or read back from a run this turn already had) and left without
      // T3 confirming it ended (a completed run, or a stop T3 reported) leaves
      // the thread uncertain, however the turn exited.
      const reached = progress.dispatched || watch.run() !== undefined;
      if (reached && !ceased(settled, progress.stop, watch)) unconfirmed(binding);
      executing.delete(request.correlation_id);
    }
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

  function bindThread(request: TurnRequest): ThreadBinding {
    return {
      threadId: derivedId(
        "thread",
        options.projectId,
        request.interface_id,
        request.agent_id,
        request.audience_id,
      ),
      projectId: options.projectId,
      title: `${options.title ?? "Carillon"} ${request.interface_id} ${request.audience_id}`.slice(
        0,
        120,
      ),
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
  ): Promise<{ readonly outcome: Outcome; readonly stop?: Stop }> {
    let text: string | undefined;
    let attempt = 0;
    for (;;) {
      let connection: T3Connection;
      try {
        connection = await connect(options.endpoint, { timeoutMs });
      } catch (failure) {
        if (attempt > 0 && attempt < reconnect.attempts && failure instanceof T3Failure) {
          attempt += 1;
          await pause();
          continue;
        }
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
          const run = watch.run();
          throw new ExplainedFailure(
            502,
            `Lost contact with T3 while the turn ran; native run ${run?.id ?? "(not recorded)"} may still be running.`,
          );
        }
        throw unavailable(failure);
      } finally {
        connection.close();
      }
    }

    /**
     * Waits before another connection. Until the turn may have reached T3,
     * a cancelled caller ends the wait at once; after a possible dispatch
     * the wait is kept, since the native work must still be found and
     * stopped.
     */
    async function pause(): Promise<void> {
      if (progress.dispatched) {
        await sleep(reconnect.delayMs);
        return;
      }
      await sleep(reconnect.delayMs, undefined, { signal }).catch(() => undefined);
      signal.throwIfAborted();
    }

    async function drive(
      connection: T3Connection,
    ): Promise<{ readonly outcome: Outcome; readonly stop?: Stop }> {
      await ensureThread(connection, binding);
      const stream = subscribe(connection, binding.threadId, watch);
      try {
        // Before a possible dispatch, a cancelled caller need not wait for
        // the snapshot; afterwards the state must be read back regardless.
        await settle(
          watch,
          stream,
          (turn) => turn.synchronized(),
          timeoutMs,
          "the thread snapshot",
          progress.dispatched ? undefined : signal,
        );
        if (watch.run() === undefined && !signal.aborted) {
          await stopBlocking(connection, binding, watch, stream);
          text ??= await compose(request, watch.empty());
          progress.dispatched = true;
          await dispatch(connection, binding, request, text);
        }
        const outcome = await observe(watch, stream, signal);
        if (outcome === undefined) {
          progress.stop = await cancel(connection, binding, watch, stream, progress.dispatched);
          throw signal.reason instanceof Error
            ? signal.reason
            : new Error("turn cancelled", { cause: signal.reason });
        }
        if (outcome.kind === "blocked" || outcome.kind === "unresolved") {
          progress.stop = await cancel(connection, binding, watch, stream, progress.dispatched);
          return { outcome, stop: progress.stop };
        }
        return { outcome };
      } finally {
        stream.end();
      }
    }
  }

  async function ensureThread(connection: T3Connection, binding: ThreadBinding): Promise<void> {
    try {
      await connection.call(RPC.dispatchCommand, threadCreate(binding, options.selection));
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
      (item) => watch.apply(item),
    );
  }

  /** Earlier turns' runs still occupying the thread are stale to Carillon: stop them first. */
  async function stopBlocking(
    connection: T3Connection,
    binding: ThreadBinding,
    watch: TurnWatch,
    stream: T3Stream,
  ): Promise<void> {
    const blocking = watch.blocking();
    if (blocking.length === 0) return;
    for (const run of blocking) {
      log(`thread ${binding.threadId}: stopping stale run ${run.id} (${run.status})`);
      await interrupt(connection, binding.threadId, run.id, "Carillon moved on to a later turn");
    }
    if (!(await settle(watch, stream, (turn) => turn.blocking().length === 0, settleMs)))
      throw new ExplainedFailure(
        503,
        "An earlier turn is still running on T3 and could not be stopped; try again shortly.",
      );
  }

  async function dispatch(
    connection: T3Connection,
    binding: ThreadBinding,
    request: TurnRequest,
    text: string,
  ): Promise<void> {
    let result: unknown;
    try {
      result = await connection.call(
        RPC.dispatchCommand,
        messageDispatch(binding.threadId, request.correlation_id, text, options.selection),
      );
    } catch (failure) {
      if (failure instanceof T3Failure && failure.kind === "rejected")
        throw new ExplainedFailure(
          502,
          `T3 refused the message: ${failure.detail ?? failure.message}`,
        );
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
    const accepted = await interrupt(
      connection,
      binding.threadId,
      target.id,
      "Carillon cancelled the turn",
    );
    // With nothing running, T3 refuses the interrupt, yet the process may
    // still owe a report (or T3 cannot show whether it does). The only public
    // stop left is a provider-session detach. T3 accepts it, removes the
    // session's record and closes the process afterwards, without fencing
    // what that process may still deliver, so the acceptance is a request
    // receipt, never a confirmed stop.
    const session = watch.providerSession();
    if (!accepted && watch.activeRun() === undefined && session !== undefined) {
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
          `thread ${binding.threadId}: session detach not accepted (${failure.detail ?? failure.message})`,
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
      log(`run ${runId}: interrupt not accepted (${failure.detail ?? failure.message})`);
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

  /** The person's text, with the agent's instructions before the thread's first message. */
  async function compose(request: TurnRequest, firstMessage: boolean): Promise<string> {
    const parts: string[] = [];
    if (firstMessage) {
      const instructions = await agentInstructions(request.resource_root);
      if (instructions !== undefined) parts.push(instructions);
    }
    parts.push(request.input);
    const earlier = (request.unanswered ?? []).flatMap((turn) =>
      turn.attachments.map((attachment) => attachment.name),
    );
    if (earlier.length > 0)
      parts.push(
        `[Carillon: ${String(earlier.length)} file(s) attached to earlier messages are not available to this runtime: ${earlier.join(", ")}]`,
      );
    return parts.join("\n\n");
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
          `T3 rejected the turn: ${failure.detail ?? failure.message}`,
        );
      case "malformed":
        return new ExplainedFailure(502, "T3 answered in a form this runtime does not understand.");
      case "disconnected":
      case "timeout":
        return new ExplainedFailure(502, `Lost contact with T3: ${failure.message}.`);
    }
  }
}

function asError(failure: unknown): Error {
  return failure instanceof Error ? failure : new Error(String(failure));
}

/**
 * The agent's instructions from the resource snapshot, for the first message
 * of a thread. T3 has no system-prompt facility for a thread, so they travel
 * as a delimited preamble; the snapshot's extensions and skills are Pi's and
 * are not loaded here.
 */
async function agentInstructions(resourceRoot: string): Promise<string | undefined> {
  const files = await Promise.all(
    ["SYSTEM.md", "AGENTS.md"].map((name) =>
      readFile(join(resourceRoot, name), "utf8").then(
        (text) => text.trim(),
        (failure: unknown) => {
          if (failure instanceof Error && "code" in failure && failure.code === "ENOENT") return "";
          throw failure;
        },
      ),
    ),
  );
  const present = files.filter((text) => text.length > 0);
  if (present.length === 0) return undefined;
  return `[Agent instructions from Carillon]\n${present.join("\n\n")}\n[End of agent instructions]`;
}

/** The configured selection is the only one offered, at every scope. */
function fixedModelState(selection: NativeSelection, query: ModelQuery): ModelState {
  const chosen = {
    provider: selection.instanceId,
    model: selection.model,
    options: Object.fromEntries(selection.options.map(({ id, value }) => [id, value])),
  };
  const scope = { selection: chosen, overridden: false };
  return {
    available: [
      {
        provider: selection.instanceId,
        model: selection.model,
        name: selection.model,
        options: selection.options.map(({ id, value }) => ({
          id,
          name: id,
          choices: [value],
          default: value,
        })),
      },
    ],
    agent: scope,
    ...(query.audience_id === undefined ? {} : { conversation: scope }),
  };
}
