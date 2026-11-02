/** A small stand-in for core's runtime API, for runtimes' tests. */
import { once } from "node:events";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { Check, Parse } from "typebox/value";
import {
  TurnOpeningSchema,
  TurnOutcomeSchema,
  type RunningTurn,
  type RuntimeTurnRequest,
  type TurnOutcome,
} from "@carillon/protocol";

/** A call core received. */
export interface Received {
  readonly method: string;
  readonly path: string;
  readonly provider: string | undefined;
  readonly authorization: string | undefined;
  readonly body: unknown;
}

export interface FakeCore {
  readonly url: string;
  /** Every call core received, in order. */
  readonly received: readonly Received[];
  /** Answers the next calls with these statuses, in order, before answering as core does. */
  answer(...statuses: number[]): void;
  /** Records a turn core handed the runtime, which core lists running until it is settled. */
  hold(request: RuntimeTurnRequest): void;
  /** Resolves with the outcome the runtime first settles `turnId` with. */
  outcome(turnId: string): Promise<TurnOutcome>;
  /**
   * Holds the answers to calls of `path`, which core has acted on, until
   * released: as if the caller stopped before it heard them.
   */
  stall(path: string): { readonly reached: Promise<void>; release(): void };
  close(): Promise<void>;
}

/**
 * Settles each turn once, answering the same outcome again as recorded and
 * another as superseded, opens one turn per runtime and key, and lists the
 * turns held or opened that are not settled yet, oldest first.
 */
export async function startFakeCore(port = 0): Promise<FakeCore> {
  const received: Received[] = [];
  const scripted: number[] = [];
  const settled = new Map<string, unknown>();
  const opened = new Map<string, string>();
  const running = new Map<string, RunningTurn>();
  const outcomes = new Map<string, PromiseWithResolvers<TurnOutcome>>();
  const outcomeOf = (turnId: string) => {
    const known = outcomes.get(turnId) ?? Promise.withResolvers<TurnOutcome>();
    outcomes.set(turnId, known);
    return known;
  };
  const stalls = new Map<string, { reached: () => void; released: Promise<void> }>();
  const server = createServer((request, response) => {
    void respond(request, response);
  });
  async function respond(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const [status, body] = await answer(request);
    const stall = stalls.get(request.url ?? "/");
    if (stall !== undefined) {
      stall.reached();
      await stall.released;
    }
    reply(response, status, body);
  }
  async function answer(request: IncomingMessage): Promise<[number, unknown?]> {
    let text = "";
    for await (const chunk of request) text += String(chunk);
    const path = request.url ?? "/";
    const provider = request.headers["x-carillon-provider"];
    const body: unknown = text.length === 0 ? undefined : JSON.parse(text);
    received.push({
      method: request.method ?? "",
      path,
      provider: typeof provider === "string" ? provider : undefined,
      authorization: request.headers.authorization,
      body,
    });
    const status = scripted.shift();
    if (status !== undefined) return [status, { error: `answered ${String(status)}` }];
    const outcome = /^\/v1\/runtime\/turns\/([^/]+)\/outcome$/u.exec(path)?.[1];
    if (outcome !== undefined) {
      const kind = kindOf(body);
      const earlier = settled.get(outcome);
      if (earlier === undefined) settled.set(outcome, kind);
      const turnId = decodeURIComponent(outcome);
      running.delete(turnId);
      if (earlier === undefined && Check(TurnOutcomeSchema, body))
        outcomeOf(turnId).resolve(Parse(TurnOutcomeSchema, body));
      return [earlier === undefined || earlier === kind ? 204 : 409];
    }
    if (path === "/v1/runtime/turns" && request.method === "GET")
      return [200, [...running.values()]];
    if (path === "/v1/runtime/turns") {
      const event = `runtime:${String(provider)}:${String(keyOf(body))}`;
      const known = opened.get(event);
      const turnId = known ?? `opened-${String(opened.size + 1)}`;
      opened.set(event, turnId);
      if (known === undefined && Check(TurnOpeningSchema, body))
        running.set(turnId, {
          request: {
            correlation_id: turnId,
            interface_id: body.interface_id,
            agent_id: body.agent_id,
            audience_id: body.audience_id,
            channel_id: "300",
            input: body.label ?? "",
            native: {},
            resource_root: "",
            attachment_root: "/",
          },
          key: body.key,
        });
      // A turn settled since it opened is no longer running.
      const state = settled.has(turnId) ? "completed" : "running";
      return [
        200,
        {
          event_id: event,
          turn_id: turnId,
          state,
          duplicate: known !== undefined,
          channel_id: "300",
        },
      ];
    }
    return [404, { error: "not found" }];
  }
  server.listen(port, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("expected a TCP listener");
  return {
    url: `http://127.0.0.1:${String(address.port)}`,
    received,
    answer(...statuses) {
      scripted.push(...statuses);
    },
    hold(request) {
      running.set(request.correlation_id, { request });
    },
    outcome: (turnId) => outcomeOf(turnId).promise,
    stall(path) {
      const reached = Promise.withResolvers<void>();
      const released = Promise.withResolvers<void>();
      stalls.set(path, { reached: reached.resolve, released: released.promise });
      return {
        reached: reached.promise,
        release() {
          stalls.delete(path);
          released.resolve();
        },
      };
    },
    async close() {
      await new Promise((done) => server.close(done));
    },
  };
}

function kindOf(body: unknown): unknown {
  return typeof body === "object" && body !== null && "outcome" in body ? body.outcome : undefined;
}

function keyOf(body: unknown): unknown {
  return typeof body === "object" && body !== null && "key" in body ? body.key : undefined;
}

function reply(response: ServerResponse, status: number, body?: unknown): void {
  if (body === undefined) {
    response.writeHead(status).end();
    return;
  }
  const text = JSON.stringify(body);
  response.writeHead(status, { "content-type": "application/json" }).end(text);
}
