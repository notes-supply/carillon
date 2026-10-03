/**
 * One authenticated WebSocket to a T3 server speaking orchestration protocol
 * 2: answered calls and acknowledged streams over Effect's RPC framing. The
 * server is checked for the verified protocol before the credential is sent,
 * and every answer is classified: an incompatible or unreachable server, a
 * refused upgrade, a closed socket, a deadline, a rejection or an answer this
 * runtime does not understand are distinct failures, never a guess.
 */
import { ORCHESTRATION_PROTOCOL } from "./contracts.ts";

export type T3FailureKind =
  | "unreachable"
  | "incompatible"
  | "refused"
  | "disconnected"
  | "timeout"
  | "rejected"
  | "malformed";

export class T3Failure extends Error {
  readonly kind: T3FailureKind;
  /** The server's own words for a rejection, when it gave any. */
  readonly detail: string | undefined;

  constructor(kind: T3FailureKind, message: string, detail?: string) {
    super(message);
    this.kind = kind;
    this.detail = detail;
  }
}

export interface T3Endpoint {
  /** The server's HTTP origin, such as `http://127.0.0.1:3773`. */
  readonly url: string;
  readonly token: string;
}

export interface T3Stream {
  /** Resolves when the stream ends, rejects when it fails or the socket closes. */
  readonly done: Promise<void>;
  /** Stops the stream; its settled `done` is then resolved. */
  end(): void;
}

export interface T3Connection {
  call(tag: string, payload: unknown): Promise<unknown>;
  subscribe(tag: string, payload: unknown, onItem: (item: unknown) => void): T3Stream;
  /** Resolves once the socket is closed, by either side. */
  readonly closed: Promise<void>;
  close(): void;
}

export interface ConnectOptions {
  /** Bounds discovery, the upgrade and every answered call. */
  readonly timeoutMs?: number;
}

export type Connect = (endpoint: T3Endpoint, options?: ConnectOptions) => Promise<T3Connection>;

export const DEFAULT_TIMEOUT_MS = 30_000;

/** Opens a connection, or fails naming why the server cannot be used. */
export const connectT3: Connect = async (endpoint, options = {}) => {
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  await checkProtocol(endpoint.url, timeoutMs);
  const url = new URL("/ws", endpoint.url);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.searchParams.set("orchestrationProtocol", String(ORCHESTRATION_PROTOCOL));
  const socket = new WebSocket(url, { headers: { authorization: `Bearer ${endpoint.token}` } });
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => {
      socket.close();
      reject(new T3Failure("unreachable", "T3 did not accept the connection in time"));
    }, timeoutMs);
    socket.addEventListener("open", () => {
      clearTimeout(timer);
      resolve();
    });
    socket.addEventListener("close", (event) => {
      clearTimeout(timer);
      // A refused upgrade (such as a bad credential) and a dead peer both end
      // in a close before open; the server does not say which.
      reject(
        new T3Failure("refused", `T3 refused the connection (close code ${String(event.code)})`),
      );
    });
  });
  return openConnection(socket, timeoutMs);
};

/** The server's protocol discovery answer must name the verified version. */
async function checkProtocol(origin: string, timeoutMs: number): Promise<void> {
  let response: Response;
  try {
    response = await fetch(new URL("/ws", origin), {
      signal: AbortSignal.timeout(timeoutMs),
      redirect: "manual",
    });
  } catch (failure) {
    throw new T3Failure(
      "unreachable",
      `T3 is unreachable: ${failure instanceof Error ? failure.message : String(failure)}`,
    );
  }
  let version: unknown;
  try {
    const body: unknown = await response.json();
    version = isRecord(body) ? body["orchestrationProtocolVersion"] : undefined;
  } catch {
    version = undefined;
  }
  if (response.status !== 426 || typeof version !== "number")
    throw new T3Failure(
      "incompatible",
      `T3 did not answer protocol discovery as expected (HTTP ${String(response.status)})`,
    );
  if (version !== ORCHESTRATION_PROTOCOL)
    throw new T3Failure(
      "incompatible",
      `T3 speaks orchestration protocol ${String(version)}; this runtime supports ${String(ORCHESTRATION_PROTOCOL)}`,
    );
}

interface Pending {
  chunk(values: readonly unknown[]): void;
  exit(failure: T3Failure | undefined, value: unknown): void;
}

function openConnection(socket: WebSocket, timeoutMs: number): T3Connection {
  const pending = new Map<string, Pending>();
  let next = 0;
  let closeFailure: T3Failure | undefined;
  const closed = new Promise<void>((resolve) => {
    socket.addEventListener("close", () => {
      closeFailure ??= new T3Failure("disconnected", "the T3 connection closed");
      for (const request of [...pending.values()]) request.exit(closeFailure, undefined);
      pending.clear();
      resolve();
    });
  });
  socket.addEventListener("message", (event) => {
    let frames: unknown;
    try {
      frames = JSON.parse(typeof event.data === "string" ? event.data : "");
    } catch {
      abandon(new T3Failure("malformed", "T3 sent a frame that is not JSON"));
      return;
    }
    for (const frame of Array.isArray(frames) ? frames : [frames]) {
      try {
        handle(frame);
      } catch (failure) {
        abandon(
          failure instanceof T3Failure
            ? failure
            : new T3Failure("malformed", "T3 sent a frame this runtime does not understand"),
        );
        return;
      }
    }
  });

  function handle(frame: unknown): void {
    if (!isRecord(frame) || typeof frame["_tag"] !== "string")
      throw new T3Failure("malformed", "T3 sent a frame without a tag");
    switch (frame["_tag"]) {
      case "Ping":
        send({ _tag: "Pong" });
        return;
      case "Chunk": {
        const request = pendingFor(frame);
        const values = frame["values"];
        if (!Array.isArray(values))
          throw new T3Failure("malformed", "T3 sent a chunk without values");
        send({ _tag: "Ack", requestId: frame["requestId"] });
        request?.chunk(values);
        return;
      }
      case "Exit": {
        const request = pendingFor(frame);
        const [failure, value] = exitOutcome(frame["exit"]);
        request?.exit(failure, value);
        return;
      }
      case "Defect":
        throw new T3Failure(
          "rejected",
          "T3 reported an internal defect",
          describe(frame["defect"]),
        );
      default:
        return;
    }
  }

  function pendingFor(frame: Record<string, unknown>): Pending | undefined {
    const id = frame["requestId"];
    return typeof id === "string" ? pending.get(id) : undefined;
  }

  function abandon(failure: T3Failure): void {
    closeFailure = failure;
    socket.close();
  }

  function send(frame: unknown): void {
    socket.send(JSON.stringify(frame));
  }

  function request(tag: string, payload: unknown, handler: Pending): string {
    if (socket.readyState !== WebSocket.OPEN) {
      queueMicrotask(() =>
        handler.exit(new T3Failure("disconnected", "the T3 connection is closed"), undefined),
      );
      return "";
    }
    next += 1;
    const id = String(next);
    pending.set(id, handler);
    send({ _tag: "Request", id, tag, payload, headers: [] });
    return id;
  }

  return {
    closed,
    close() {
      socket.close();
    },
    call(tag, payload) {
      return new Promise((resolve, reject) => {
        let latest: unknown;
        const id = request(tag, payload, {
          chunk(values) {
            latest = values.at(-1);
          },
          exit(failure, value) {
            clearTimeout(timer);
            pending.delete(id);
            if (failure !== undefined) reject(failure);
            else resolve(value ?? latest);
          },
        });
        const timer = setTimeout(() => {
          pending.delete(id);
          reject(
            new T3Failure("timeout", `T3 did not answer ${tag} within ${String(timeoutMs)} ms`),
          );
        }, timeoutMs);
      });
    },
    subscribe(tag, payload, onItem) {
      let id = "";
      let finish: (failure: T3Failure | undefined) => void = () => undefined;
      const done = new Promise<void>((resolve, reject) => {
        finish = (failure) => {
          pending.delete(id);
          if (failure === undefined) resolve();
          else reject(failure);
        };
      });
      id = request(tag, payload, {
        chunk(values) {
          for (const value of values) onItem(value);
        },
        exit(failure) {
          finish(failure);
        },
      });
      return {
        done,
        end() {
          if (pending.has(id)) send({ _tag: "Interrupt", requestId: id });
          finish(undefined);
        },
      };
    },
  };
}

/** The failure an exit carries, or the value it succeeded with. */
function exitOutcome(exit: unknown): [T3Failure | undefined, unknown] {
  if (!isRecord(exit)) throw new T3Failure("malformed", "T3 sent an exit without an outcome");
  if (exit["_tag"] === "Success") return [undefined, exit["value"]];
  if (exit["_tag"] !== "Failure" || !Array.isArray(exit["cause"]))
    throw new T3Failure("malformed", "T3 sent an exit this runtime does not understand");
  for (const entry of exit["cause"]) {
    if (!isRecord(entry)) continue;
    if (entry["_tag"] === "Fail") {
      const error = entry["error"];
      const tag = isRecord(error) && typeof error["_tag"] === "string" ? error["_tag"] : "failure";
      return [
        new T3Failure("rejected", `T3 rejected the request (${tag})`, describe(error)),
        undefined,
      ];
    }
    if (entry["_tag"] === "Die")
      return [
        new T3Failure("rejected", "T3 failed the request", describe(entry["defect"])),
        undefined,
      ];
    if (entry["_tag"] === "Interrupt")
      return [new T3Failure("rejected", "T3 interrupted the request"), undefined];
  }
  return [new T3Failure("rejected", "T3 failed the request"), undefined];
}

/** The human part of a server error: its message, detail and cause, nothing structural. */
function describe(error: unknown): string | undefined {
  if (typeof error === "string") return error;
  if (!isRecord(error)) return undefined;
  const parts = ["message", "detail", "cause"]
    .map((key) => {
      const value = error[key];
      return typeof value === "string" ? value : isRecord(value) ? describe(value) : undefined;
    })
    .filter((part): part is string => part !== undefined && part.length > 0);
  return parts.length === 0 ? undefined : parts.join(": ");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
