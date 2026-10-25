/** A durable runtime's calls to core, with its own registry credential. */
import { Type } from "typebox";
import { Check, Parse } from "typebox/value";
import {
  PROVIDER_HEADER,
  RunningTurnSchema,
  TurnReceiptSchema,
  type RunningTurn,
  type TurnOpening,
  type TurnOutcome,
  type TurnReceipt,
} from "@carillon/protocol";

/**
 * What core made of an outcome: `recorded` now or as the same outcome
 * before, `superseded` when the turn already ended another way, `unknown`
 * when this runtime holds no such turn.
 */
export type Settled = "recorded" | "superseded" | "unknown";

/** Core answered with a status that means none of those, such as 503 or 413, or opened or listed no turn. */
export class CoreRefusal extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

/**
 * Core answered the list of running turns with something other than a list.
 * Asking again reads the same until core or the runtime is replaced, as by
 * an upgrade.
 */
export class UnreadableListing extends Error {}

export interface CoreClient {
  /**
   * Settles a turn this runtime holds, in one attempt. Anything but what
   * core made of the outcome throws, an unreachable core included; the
   * caller decides when to ask again.
   */
  readonly settle: (turnId: string, outcome: TurnOutcome) => Promise<Settled>;
  /**
   * Opens a running turn in one of this runtime's conversations, or finds
   * the one its key opened. `signal` ends the wait for core's answer.
   */
  readonly open: (opening: TurnOpening, signal?: AbortSignal) => Promise<TurnReceipt>;
  /**
   * The turns core records running on this runtime, for a process that
   * forgot them to take up again. An entry it cannot read is set apart, so
   * the others are still taken up; a list it cannot read throws
   * {@link UnreadableListing}.
   */
  readonly running: () => Promise<RunningTurns>;
}

/** What core lists running on this runtime. */
export interface RunningTurns {
  /** The turns, oldest first. */
  readonly turns: readonly RunningTurn[];
  readonly unreadable: readonly UnreadableTurn[];
}

/** An entry of that list this runtime cannot read: why, and its turn's ID when it names one. */
export interface UnreadableTurn {
  readonly turnId?: string;
  readonly problem: string;
}

interface CoreClientOptions {
  readonly url: string;
  readonly providerId: string;
  readonly token: string;
}

/** How long one call may take before it is taken as not answered. */
const CALL_TIMEOUT_MS = 10_000;

const ListSchema = Type.Array(Type.Unknown());
/** As much of a running turn as names it. */
const NamedTurnSchema = Type.Object({
  request: Type.Object({ correlation_id: Type.String({ minLength: 1 }) }),
});

export function createCoreClient(options: CoreClientOptions): CoreClient {
  /** A POST of `body`, or a GET without one. */
  const call = (path: string, body?: unknown, signal?: AbortSignal) =>
    fetch(`${options.url}${path}`, {
      method: body === undefined ? "GET" : "POST",
      headers: {
        authorization: `Bearer ${options.token}`,
        [PROVIDER_HEADER]: options.providerId,
        ...(body === undefined ? {} : { "content-type": "application/json" }),
      },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      signal: AbortSignal.any([
        AbortSignal.timeout(CALL_TIMEOUT_MS),
        ...(signal === undefined ? [] : [signal]),
      ]),
    });
  return {
    settle: async (turnId, outcome) => {
      const response = await call(
        `/v1/runtime/turns/${encodeURIComponent(turnId)}/outcome`,
        outcome,
      );
      if (response.status === 204) return "recorded";
      if (response.status === 409) return "superseded";
      if (response.status === 404) return "unknown";
      throw new CoreRefusal(
        response.status,
        `core did not record the outcome of turn ${turnId}: ${await refusal(response)}`,
      );
    },
    open: async (opening, signal) => {
      const response = await call("/v1/runtime/turns", opening, signal);
      if (!response.ok)
        throw new CoreRefusal(response.status, `core opened no turn: ${await refusal(response)}`);
      return Parse(TurnReceiptSchema, await response.json());
    },
    running: async () => {
      const response = await call("/v1/runtime/turns");
      if (!response.ok)
        throw new CoreRefusal(
          response.status,
          `core did not list the turns it records running: ${await refusal(response)}`,
        );
      const listed: unknown = await response.json();
      if (!Check(ListSchema, listed))
        throw new UnreadableListing(
          "core listed the turns it records running in a form this runtime cannot read",
        );
      const turns: RunningTurn[] = [];
      const unreadable: UnreadableTurn[] = [];
      for (const entry of listed) {
        try {
          turns.push(Parse(RunningTurnSchema, entry));
        } catch (failure) {
          unreadable.push({
            ...(Check(NamedTurnSchema, entry) ? { turnId: entry.request.correlation_id } : {}),
            problem: failure instanceof Error ? failure.message : String(failure),
          });
        }
      }
      return { turns, unreadable };
    },
  };
}

/** Core's explanation of a refusal, or its status when it gave none. */
async function refusal(response: Response): Promise<string> {
  const body: unknown = await response.json().catch(() => undefined);
  const error =
    typeof body === "object" && body !== null && "error" in body ? body.error : undefined;
  return typeof error === "string" && error.length > 0
    ? error
    : `status ${String(response.status)}`;
}
