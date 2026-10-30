/**
 * The turns this runtime holds for core: each message core hands over, and
 * each report of work a message delegated, runs on T3 until it answers and
 * then settles through core. A delegated report opens a turn of its own,
 * keyed by T3's records, before the message that started it settles, so a
 * restart never loses it. Core records which turns run: a process that
 * starts reads them back and takes each up again, reading a message's run
 * back from T3 rather than running it twice. Closing the runtime leaves
 * T3's work running; a person's stop interrupts it. A stop core has not
 * recorded yet is kept in a file, so the next process settles it rather
 * than running the message again; until the file or core keeps it, the
 * runtime is not ready to restart.
 */
import { readFile } from "node:fs/promises";
import { setTimeout as sleep } from "node:timers/promises";
import type { ControlAction, TurnActivity, TurnOutcome } from "@carillon/protocol";
import { CoreRefusal, UnreadableListing, type CoreClient } from "@carillon/runtime/core";
import {
  diagnostic,
  displayableExplanation,
  ExplainedFailure,
  reportOf,
} from "@carillon/runtime/failure";
import type { RestartReadiness, TurnRequest } from "@carillon/runtime/operations";
import { publish } from "@carillon/runtime/publish";
import { Serial } from "@carillon/runtime/serial";
import { Type } from "typebox";
import { Check } from "typebox/value";
import type { FollowUp } from "./watch.ts";

/**
 * What a turn's run on T3 answered, and where the report of work it
 * delegated continues; with a report to follow, the output may be blank.
 */
export interface Answer {
  readonly output: string;
  readonly activity?: TurnActivity;
  readonly followUp?: FollowUp;
}

export interface HeldOptions {
  readonly core: Pick<CoreClient, "settle" | "open" | "running">;
  /**
   * Runs a turn on T3 until it answers: a message, or with `follow` the
   * report of work an earlier message delegated. A failure rejects; `stop`
   * stops the turn's native work, while `closing` only stops following it.
   */
  readonly run: (
    request: TurnRequest,
    follow: FollowUp | undefined,
    stop: AbortSignal,
  ) => Promise<Answer>;
  /** What `run` shows of a turn it follows on T3 for that interface. */
  readonly following: (turnId: string, interfaceId: string) => TurnActivity | undefined;
  readonly closing: AbortSignal;
  /** Keeps work that closing the runtime waits for. */
  readonly track: <T>(work: () => Promise<T>) => Promise<T>;
  /** How long the first retry of a call core did not answer waits. */
  readonly retryMs: number;
  readonly stops: StopsFile;
}

/** The file that keeps the stops core has not recorded yet, as this process read it at start. */
export interface StopsFile {
  readonly path: string;
  /** The stops an earlier process kept, for this one to settle before it runs anything. */
  readonly kept: readonly string[];
}

export interface HeldTurns {
  readonly submit: (request: TurnRequest) => Promise<void>;
  readonly control: (turnId: string, interfaceId: string, action: ControlAction) => Promise<void>;
  readonly activity: (turnId: string, interfaceId: string) => TurnActivity | undefined;
  /**
   * Whether a restart now would lose a person's stop: one T3 has yet to
   * confirm, which only this process would carry out, or one that neither
   * core nor the stops file keeps yet.
   */
  readonly restartReadiness: () => RestartReadiness;
}

/** A turn from its hand-over or opening until core has its outcome. */
interface Held {
  readonly interfaceId: string;
  /** It reports work a message delegated, which is T3's: nothing of it can be stopped. */
  readonly report: boolean;
  readonly stop: AbortController;
  /** Its run on T3 ended: nothing of it is left to stop. */
  ended: boolean;
}

const LONGEST_RETRY_MS = 60_000;
/**
 * Core refusals asking again may change: of the runtime's credential, as
 * while the runtime is disabled and core opens no turn for it, a timeout
 * and a rate limit.
 */
const TRANSIENT = new Set([401, 403, 408, 429]);
/** How long a hand-over waits for the turns an earlier process held to be taken up first, well within core's 10 s. */
const RECOVERY_WAIT_MS = 3_000;
/** How long a settled turn is remembered, for a hand-over of it already on its way. */
const SETTLED_MS = 5 * 60_000;
/** What an opened report's turn is for, recorded as its input. */
const REPORT = "The report of work delegated in T3.";
/** A held turn before its run shows on T3: it waits behind its conversation's earlier turns. */
const WAITING: TurnActivity = { phase: "queued", running: [], recent: [], tool_calls: 0 };
const StopsSchema = Type.Array(Type.String({ minLength: 1 }));

/** The key a delegated report's turn is opened under, from T3's records of the work. */
function reportKey(followUp: FollowUp): string {
  return `t3:${followUp.message}:${String(followUp.after)}`;
}

function followUpOf(key: string): FollowUp | undefined {
  const [, message, after] = /^t3:(.+):(\d+)$/u.exec(key) ?? [];
  return message === undefined || after === undefined
    ? undefined
    : { message, after: Number(after) };
}

/**
 * Reads the stops an earlier process kept at `path`; there are none without
 * the file. A file that does not read as such refuses the start, rather
 * than run a message a person stopped.
 */
export async function readStops(path: string): Promise<StopsFile> {
  const refused = (why: string) =>
    new Error(
      `${path}: cannot read the stops runtime-t3 keeps (${why}), so it does not start, rather than run a message a person stopped. Make the file readable as a JSON list of turn IDs, or remove it to start without them.`,
    );
  let kept: unknown;
  try {
    kept = JSON.parse(await readFile(path, "utf8"));
  } catch (failure) {
    if (failure instanceof Error && "code" in failure && failure.code === "ENOENT")
      return { path, kept: [] };
    throw refused(diagnostic(failure));
  }
  if (!Check(StopsSchema, kept)) throw refused("it is not a list of turn IDs");
  return { path, kept };
}

export function holdTurns(options: HeldOptions): HeldTurns {
  const held = new Map<string, Held>();
  /** Turns that ended stopped and core has not recorded yet. */
  const stops = new Set(options.stops.kept);
  /** The stops the file was last written with, durably. */
  let kept: ReadonlySet<string> = new Set(options.stops.kept);
  /** Turns settled lately; each is forgotten after {@link SETTLED_MS}. */
  const settled = new Set<string>();
  const writes = new Serial();
  const recovered = options.track(recover).catch((failure: unknown) => {
    console.error(`runtime-t3: could not take up the turns held before: ${diagnostic(failure)}`);
  });

  /** Runs and settles the turn, unless the runtime holds, stopped or settled it already. */
  function take(request: TurnRequest, follow: FollowUp | undefined): void {
    const turnId = request.correlation_id;
    if (held.has(turnId) || stops.has(turnId) || settled.has(turnId)) return;
    const turn: Held = {
      interfaceId: request.interface_id,
      report: follow !== undefined,
      stop: new AbortController(),
      ended: false,
    };
    held.set(turnId, turn);
    options
      .track(async () => {
        try {
          const outcome = await answer(request, follow, turn);
          if (outcome === undefined) return;
          if (outcome.outcome === "stopped") stops.add(turnId);
          await settle(turnId, outcome);
        } finally {
          held.delete(turnId);
        }
      })
      .catch((failure: unknown) => {
        console.error(`runtime-t3: turn ${turnId}: ${diagnostic(failure)}`);
      });
  }

  /** How the turn ended, or undefined when the runtime closed first and core keeps it running. */
  async function answer(
    request: TurnRequest,
    follow: FollowUp | undefined,
    turn: Held,
  ): Promise<TurnOutcome | undefined> {
    const stop = turn.stop.signal;
    let answered: Answer;
    try {
      answered = await options.run(request, follow, stop);
    } catch (failure) {
      // The stop's own reason says it stopped, as T3 confirmed, even while
      // the runtime closes; any other failure explains what T3 did instead.
      if (stop.aborted && failure === stop.reason) return { outcome: "stopped" };
      if (options.closing.aborted) return undefined;
      if (!(failure instanceof ExplainedFailure))
        console.error(`runtime-t3: turn ${request.correlation_id}: ${diagnostic(failure)}`);
      const explanation =
        failure instanceof ExplainedFailure ? displayableExplanation(failure.message) : undefined;
      const activity = reportOf(failure);
      return {
        outcome: "failed",
        ...(explanation === undefined ? {} : { explanation }),
        ...(activity === undefined ? {} : { activity }),
      };
    } finally {
      turn.ended = true;
    }
    const { output, activity, followUp } = answered;
    const text = followUp === undefined ? output : await report(request, followUp, output);
    if (text === undefined) return undefined;
    return { outcome: "answered", output: text, ...(activity === undefined ? {} : { activity }) };
  }

  /**
   * Opens the turn that carries the report of work `request`'s turn
   * delegated, takes it, and gives the answer that says where the report
   * is; or undefined when the runtime closes first, for the next process to
   * open it. A refusal asking again cannot change leaves the work to T3.
   */
  async function report(
    request: TurnRequest,
    followUp: FollowUp,
    output: string,
  ): Promise<string | undefined> {
    const { interface_id, agent_id, audience_id } = request;
    const receipt = await persist(`open the report of turn ${request.correlation_id}`, () =>
      options.core.open(
        { interface_id, agent_id, audience_id, key: reportKey(followUp), label: REPORT },
        options.closing,
      ),
    );
    if (receipt === undefined) {
      if (options.closing.aborted) return undefined;
      const unscheduled =
        "The work this message started goes on in T3, but Carillon could not schedule its report here; T3 shows its result.";
      return output.trim().length === 0 ? unscheduled : `${output}\n\n${unscheduled}`;
    }
    if (receipt.state === "running")
      take(
        {
          correlation_id: receipt.turn_id,
          interface_id,
          agent_id,
          audience_id,
          input: REPORT,
          native: {},
          resource_root: request.resource_root,
          attachment_root: request.attachment_root,
        },
        followUp,
      );
    return output.trim().length === 0
      ? "The work this message started is still running; its report follows."
      : output;
  }

  /**
   * Settles the turn through core, asking again until core records it, then
   * lets it go, remembering for a while that it settled. Until core records
   * a stop, each attempt first keeps it in the stops file if it is not there
   * yet. A closing runtime leaves the turn to the next process: core still
   * lists it, and a kept stop stays in the stops file.
   */
  async function settle(turnId: string, outcome: TurnOutcome): Promise<void> {
    const made = await persist(`settle turn ${turnId}`, async () => {
      if (stops.has(turnId) && !kept.has(turnId)) await writeStops();
      return options.core.settle(turnId, outcome);
    });
    if (made === undefined && options.closing.aborted) return;
    if (made === "superseded" || made === "unknown")
      console.log(
        `runtime-t3: turn ${turnId}: core ${made === "unknown" ? "holds no such turn" : "had it end another way"}`,
      );
    held.delete(turnId);
    settled.add(turnId);
    setTimeout(() => settled.delete(turnId), SETTLED_MS).unref();
    if (stops.delete(turnId)) await writeStops();
  }

  /** Settles a turn this runtime cannot run as failed, saying why. */
  function fail(turnId: string, explanation: string): void {
    void options
      .track(() => settle(turnId, { outcome: "failed", explanation }))
      .catch(() => undefined);
  }

  /**
   * Settles the stops an earlier process kept, then takes up again every
   * turn core records running on this runtime, oldest first.
   */
  async function recover(): Promise<void> {
    for (const turnId of stops) await settle(turnId, { outcome: "stopped" });
    const running = await persist("read back the turns core records running", () =>
      options.core.running(),
    );
    for (const { turnId, problem } of running?.unreadable ?? []) {
      console.error(
        `runtime-t3: core lists a running turn ${turnId ?? "without an ID"} this runtime cannot read: ${problem}`,
      );
      if (turnId !== undefined)
        fail(turnId, "Carillon could not read this turn back after its runtime restarted.");
    }
    for (const { request, key } of running?.turns ?? []) {
      const follow = key === undefined ? undefined : followUpOf(key);
      if (key === undefined || follow !== undefined) take(request, follow);
      else
        fail(
          request.correlation_id,
          "Carillon could not tell which delegated work this turn reports.",
        );
    }
  }

  /**
   * Replaces the stops file with the stops core has not recorded yet, one
   * write at a time. A failure is logged, and {@link kept} still names what
   * the file holds.
   */
  async function writeStops(): Promise<void> {
    await writes
      .run(async () => {
        const writing = new Set(stops);
        await publish(options.stops.path, `${JSON.stringify([...writing])}\n`);
        kept = writing;
      })
      .catch((failure: unknown) => {
        console.error(`runtime-t3: could not write ${options.stops.path}: ${diagnostic(failure)}`);
      });
  }

  /**
   * Calls core until it answers, waiting longer after each failure, or
   * until the runtime closes. A refusal asking again cannot change ends it
   * with nothing.
   */
  async function persist<T>(what: string, call: () => Promise<T>): Promise<T | undefined> {
    let previous: unknown;
    for (let delay = options.retryMs; ; delay = Math.min(delay * 2, LONGEST_RETRY_MS)) {
      try {
        return await call();
      } catch (failure) {
        if (options.closing.aborted) return undefined;
        if (
          failure instanceof CoreRefusal &&
          failure.status < 500 &&
          !TRANSIENT.has(failure.status)
        ) {
          console.error(`runtime-t3: could not ${what}: ${diagnostic(failure)}`);
          return undefined;
        }
        // Core's list in a form this runtime cannot read stays so until one
        // of them is replaced, so it is told once.
        if (!(failure instanceof UnreadableListing && previous instanceof UnreadableListing))
          console.error(
            `runtime-t3: could not ${what}: ${diagnostic(failure)}; asking again in ${String(delay / 1_000)} s`,
          );
        previous = failure;
        await sleep(delay, undefined, { signal: options.closing }).catch(() => undefined);
      }
    }
  }

  return {
    submit: async (request) => {
      options.closing.throwIfAborted();
      const waited = new AbortController();
      const ready = await Promise.race([
        recovered.then(() => true),
        sleep(RECOVERY_WAIT_MS, false, { signal: waited.signal }),
      ]).finally(() => {
        waited.abort();
      });
      if (!ready)
        throw new ExplainedFailure(503, "The runtime is still taking up the turns it held.");
      options.closing.throwIfAborted();
      take(request, undefined);
    },
    control: async (turnId, interfaceId, action) => {
      options.closing.throwIfAborted();
      const turn = held.get(turnId);
      // A message is only ever controlled from the interface it came from.
      if (turn?.interfaceId !== interfaceId)
        throw new ExplainedFailure(404, "This runtime holds no such message.");
      if (turn.report)
        throw new ExplainedFailure(409, "The delegated work this reports is T3's, and goes on.");
      if (turn.ended) throw new ExplainedFailure(409, "That message was already answered.");
      if (action !== "stop")
        throw new ExplainedFailure(
          409,
          "On T3, a message can be stopped, not steered or interrupted.",
        );
      turn.stop.abort(new Error("a person stopped the turn"));
    },
    activity: (turnId, interfaceId) => {
      const turn = held.get(turnId);
      if (turn?.interfaceId !== interfaceId || turn.ended) return undefined;
      return {
        ...(options.following(turnId, interfaceId) ?? WAITING),
        ...(turn.report || turn.stop.signal.aborted ? {} : { controls: ["stop"] }),
      };
    },
    restartReadiness: () => {
      if ([...held.values()].some((turn) => turn.stop.signal.aborted && !turn.ended))
        return { state: "unready", reason: "a person's stop waits for T3 to confirm it" };
      if ([...stops].some((turnId) => !kept.has(turnId)))
        return {
          state: "unready",
          reason: `a person's stop is kept neither by core nor in ${options.stops.path} yet`,
        };
      return { state: "ready" };
    },
  };
}
