/** A durable runtime's calls to core, with its own registry credential. */
import { Parse } from "typebox/value";
import {
  PROVIDER_HEADER,
  TurnReceiptSchema,
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

/** Core answered with a status that means none of those, such as 503 or 413. */
export class CoreRefusal extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

export interface CoreClient {
  /**
   * Settles a turn this runtime holds, in one attempt. Anything but what
   * core made of the outcome throws, an unreachable core included; the
   * caller decides when to ask again.
   */
  readonly settle: (turnId: string, outcome: TurnOutcome) => Promise<Settled>;
  /** Opens a running turn in one of this runtime's conversations, or finds the one its key opened. */
  readonly open: (opening: TurnOpening) => Promise<TurnReceipt>;
}

interface CoreClientOptions {
  readonly url: string;
  readonly providerId: string;
  readonly token: string;
}

/** How long one call may take before it is taken as not answered. */
const CALL_TIMEOUT_MS = 10_000;

export function createCoreClient(options: CoreClientOptions): CoreClient {
  const call = (path: string, body: unknown) =>
    fetch(`${options.url}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${options.token}`,
        [PROVIDER_HEADER]: options.providerId,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(CALL_TIMEOUT_MS),
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
    open: async (opening) => {
      const response = await call("/v1/runtime/turns", opening);
      if (!response.ok) throw new Error(`core opened no turn: ${await refusal(response)}`);
      return Parse(TurnReceiptSchema, await response.json());
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
