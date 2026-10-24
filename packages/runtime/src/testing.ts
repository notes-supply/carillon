/** A small stand-in for core's runtime API, for durable runtimes' tests. */
import { once } from "node:events";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";

/** A call core received. */
export interface Received {
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
  close(): Promise<void>;
}

/**
 * Settles each turn once, answering the same outcome again as recorded and
 * another as superseded, and opens one turn per runtime and key.
 */
export async function startFakeCore(port = 0): Promise<FakeCore> {
  const received: Received[] = [];
  const scripted: number[] = [];
  const settled = new Map<string, unknown>();
  const opened = new Map<string, string>();
  const server = createServer((request, response) => {
    void respond(request, response);
  });
  async function respond(request: IncomingMessage, response: ServerResponse): Promise<void> {
    let text = "";
    for await (const chunk of request) text += String(chunk);
    const path = request.url ?? "/";
    const provider = request.headers["x-carillon-provider"];
    const body: unknown = text.length === 0 ? undefined : JSON.parse(text);
    received.push({
      path,
      provider: typeof provider === "string" ? provider : undefined,
      authorization: request.headers.authorization,
      body,
    });
    const status = scripted.shift();
    if (status !== undefined) {
      reply(response, status, { error: `answered ${String(status)}` });
      return;
    }
    const outcome = /^\/v1\/runtime\/turns\/([^/]+)\/outcome$/u.exec(path)?.[1];
    if (outcome !== undefined) {
      const kind = kindOf(body);
      const earlier = settled.get(outcome);
      if (earlier === undefined) settled.set(outcome, kind);
      reply(response, earlier === undefined || earlier === kind ? 204 : 409);
      return;
    }
    if (path === "/v1/runtime/turns") {
      const event = `runtime:${String(provider)}:${String(keyOf(body))}`;
      const known = opened.get(event);
      const turnId = known ?? `opened-${String(opened.size + 1)}`;
      opened.set(event, turnId);
      reply(response, 200, {
        event_id: event,
        turn_id: turnId,
        state: "running",
        duplicate: known !== undefined,
      });
      return;
    }
    reply(response, 404, { error: "not found" });
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
