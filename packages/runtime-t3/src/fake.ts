/**
 * A test double of T3's orchestration server: the protocol discovery, the
 * authenticated upgrade, Effect RPC framing, command receipts, thread
 * creation, message dispatch, run interruption with T3's settlement of
 * background work, provider sessions, and thread subscriptions with bounded
 * snapshots, sequence-based replay and completion markers, with faults a
 * test can inject. It executes nothing; tests script what a run does. It is
 * a test fixture, not used by the runtime itself.
 */
import { once } from "node:events";
import { createServer, type IncomingMessage } from "node:http";
import type { Duplex } from "node:stream";
import { WebSocketServer, type RawData, type WebSocket } from "ws";
import {
  ORCHESTRATION_PROTOCOL,
  type Message,
  type ProviderSession,
  type ProviderThread,
  type Run,
  type RuntimeRequest,
  type TurnItem,
} from "./contracts.ts";

/** A timeline item as the fake stores it: any kind, with whatever fields a test gives. */
export interface FakeItem {
  readonly id: string;
  readonly runId: string | null;
  readonly type: string;
  readonly status: TurnItem["status"];
  readonly ordinal: number;
  readonly title: string | null;
  readonly startedAt: string | null;
  readonly completedAt: string | null;
  readonly updatedAt: string;
  readonly [field: string]: unknown;
}

interface StoredEvent {
  readonly sequence: number;
  readonly type: string;
  readonly payload: unknown;
}

interface FakeThread {
  readonly id: string;
  readonly projectId: string;
  readonly runs: Run[];
  readonly messages: Message[];
  readonly items: FakeItem[];
  readonly requests: RuntimeRequest[];
  providerThread: ProviderThread;
  session: ProviderSession;
  sequence: number;
  readonly events: StoredEvent[];
  readonly subscribers: Set<{ readonly socket: WebSocket; readonly requestId: unknown }>;
}

export interface Command {
  readonly type: string;
  readonly commandId: string;
  readonly threadId: string;
  readonly [key: string]: unknown;
}

export interface Subscription {
  readonly afterSequence: number | undefined;
  readonly bounded: boolean;
  readonly marker: boolean;
}

export interface FakeT3 {
  readonly url: string;
  readonly token: string;
  readonly threads: Map<string, FakeThread>;
  /** Every command received, in order, before any fault applies. */
  readonly received: Command[];
  /** Every command processed, in order, including replays. */
  readonly commands: Command[];
  /** Every interrupt accepted, by run. */
  readonly interrupts: string[];
  /** Every thread subscription received, in order. */
  readonly subscriptions: Subscription[];
  /** What protocol discovery announces. */
  protocolVersion: number;
  /** Processes the command, then drops the socket before answering. */
  dropAnswer: (command: Command) => boolean;
  /** Drops the socket before processing the command. */
  dropRequest: (command: Command) => boolean;
  /** Delays the answer to a command, in milliseconds. */
  delayAnswer: (command: Command) => number;
  /** Rejects the command with this detail instead of processing it. */
  reject: (command: Command) => string | undefined;
  /** Answers the next request with a frame that is not JSON. */
  malformed: boolean;
  /** Whether an interrupt stops the run (and settles background work) or is accepted and ignored. */
  interruptBehaviour: "settle" | "ignore";
  /** Answers a resume with a snapshot instead of a replay, as T3 does when the replay is too long. */
  snapshotOnResume: boolean;
  /** Leaves a run\'s pending requests open when it ends: a contradictory projection. */
  retainRequests: boolean;
  /** Closes every client socket, as a server restart would. */
  disconnectAll(): void;
  tool(threadId: string, runId: string, item: Partial<FakeItem> & { type: string }): FakeItem;
  finishTool(threadId: string, item: FakeItem, status?: TurnItem["status"]): void;
  assistant(threadId: string, runId: string, text: string, streaming?: boolean): void;
  complete(threadId: string, runId: string): void;
  fail(threadId: string, runId: string, message: string): void;
  stop(threadId: string, runId: string, status: Run["status"]): void;
  request(
    threadId: string,
    runId: string,
    kind: string,
    item?: Partial<FakeItem> & { type: string },
  ): RuntimeRequest;
  background(threadId: string, tasks: ProviderThread["pendingBackgroundTasks"]): void;
  /** A run T3 starts itself, as when finished background work wakes the agent. */
  wake(threadId: string, text: string): Run;
  session(threadId: string, status: ProviderSession["status"]): void;
  waitFor(condition: () => boolean, timeoutMs?: number): Promise<void>;
  close(): Promise<void>;
}

const ACTIVE: ReadonlySet<Run["status"]> = new Set([
  "preparing",
  "queued",
  "starting",
  "running",
  "waiting",
]);
const BACKGROUND_ITEMS: ReadonlySet<string> = new Set([
  "command_execution",
  "dynamic_tool",
  "subagent",
]);
const RUNNING_ITEMS: ReadonlySet<string> = new Set(["pending", "running", "waiting"]);

export async function startFakeT3(token = "fake-t3-token"): Promise<FakeT3> {
  const threads = new Map<string, FakeThread>();
  const received: Command[] = [];
  const commands: Command[] = [];
  const interrupts: string[] = [];
  const subscriptions: Subscription[] = [];
  const receipts = new Map<string, number>();
  const requestRuns = new Map<string, string>();
  const sockets = new Set<WebSocket>();

  const server = createServer((request, response) => {
    if (request.url?.startsWith("/ws") === true) {
      response.writeHead(426, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          code: "orchestration_protocol_incompatible",
          orchestrationProtocolVersion: fake.protocolVersion,
        }),
      );
      return;
    }
    response.writeHead(404);
    response.end();
  });
  const wss = new WebSocketServer({ noServer: true });
  server.on("upgrade", (request: IncomingMessage, socket: Duplex, head: Buffer) => {
    const url = new URL(request.url ?? "/", "http://fake");
    if (url.searchParams.get("orchestrationProtocol") !== String(fake.protocolVersion)) {
      socket.write("HTTP/1.1 426 Upgrade Required\r\n\r\n");
      socket.destroy();
      return;
    }
    if (request.headers.authorization !== `Bearer ${token}`) {
      socket.write("HTTP/1.1 401 Unauthorized\r\n\r\n");
      socket.destroy();
      return;
    }
    wss.handleUpgrade(request, socket, head, (client) => {
      sockets.add(client);
      client.on("close", () => {
        sockets.delete(client);
        for (const thread of threads.values())
          for (const subscriber of thread.subscribers)
            if (subscriber.socket === client) thread.subscribers.delete(subscriber);
      });
      client.on("message", (data: RawData) => {
        const frame: unknown = JSON.parse(text(data));
        if (isRecord(frame)) void handle(client, frame);
      });
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("expected a TCP listener");

  const fake: FakeT3 = {
    url: `http://127.0.0.1:${String(address.port)}`,
    token,
    threads,
    received,
    commands,
    interrupts,
    subscriptions,
    protocolVersion: ORCHESTRATION_PROTOCOL,
    dropAnswer: () => false,
    dropRequest: () => false,
    delayAnswer: () => 0,
    reject: () => undefined,
    malformed: false,
    interruptBehaviour: "settle",
    snapshotOnResume: false,
    retainRequests: false,
    disconnectAll() {
      for (const socket of sockets) socket.terminate();
    },
    tool(threadId, runId, item) {
      const thread = threadOf(threadId);
      const now = new Date().toISOString();
      const full: FakeItem = {
        id: `item-${String(thread.items.length + 1)}`,
        runId,
        status: "running",
        ordinal: thread.items.length,
        title: null,
        startedAt: now,
        completedAt: null,
        updatedAt: now,
        ...item,
      };
      thread.items.push(full);
      emit(thread, "turn-item.updated", full);
      return full;
    },
    finishTool(threadId, item, status = "completed") {
      const thread = threadOf(threadId);
      const now = new Date().toISOString();
      const finished = { ...item, status, completedAt: now, updatedAt: now };
      thread.items.splice(thread.items.indexOf(item), 1, finished);
      emit(thread, "turn-item.updated", finished);
    },
    assistant(threadId, runId, text, streaming = false) {
      const thread = threadOf(threadId);
      const id = `assistant-${runId}`;
      const existing = thread.messages.findIndex((message) => message.id === id);
      const message: Message = {
        id,
        runId,
        role: "assistant",
        text,
        createdAt:
          existing === -1 ? new Date().toISOString() : (thread.messages[existing]?.createdAt ?? ""),
      };
      if (existing === -1) thread.messages.push(message);
      else thread.messages[existing] = message;
      emit(thread, "message.updated", { ...message, streaming });
    },
    complete(threadId, runId) {
      updateRun(threadId, runId, "completed");
    },
    fail(threadId, runId, message) {
      fake.tool(threadId, runId, {
        type: "error",
        status: "failed",
        failure: { class: "provider_error", message },
      });
      updateRun(threadId, runId, "failed");
    },
    stop(threadId, runId, status) {
      updateRun(threadId, runId, status);
    },
    request(threadId, runId, kind, item) {
      const thread = threadOf(threadId);
      const request: RuntimeRequest = {
        id: `request-${String(thread.requests.length + 1)}`,
        kind,
        status: "pending",
      };
      thread.requests.push(request);
      requestRuns.set(request.id, runId);
      if (item !== undefined) fake.tool(threadId, runId, { ...item, requestId: request.id });
      const run = thread.runs.find((candidate) => candidate.id === runId);
      if (run !== undefined && ACTIVE.has(run.status)) updateRun(threadId, runId, "waiting");
      emit(thread, "runtime-request.updated", request);
      return request;
    },
    background(threadId, tasks) {
      const thread = threadOf(threadId);
      const rest = {
        id: thread.providerThread.id,
        providerSessionId: thread.providerThread.providerSessionId,
      };
      thread.providerThread =
        tasks === undefined ? rest : { ...rest, pendingBackgroundTasks: tasks };
      emit(thread, "provider-thread.updated", thread.providerThread);
    },
    wake(threadId, text) {
      const thread = threadOf(threadId);
      return startRun(thread, `message:${thread.id}:wake:${String(thread.runs.length + 1)}`, text);
    },
    session(threadId, status) {
      const thread = threadOf(threadId);
      thread.session = { ...thread.session, status };
      emit(thread, "provider-session.updated", thread.session);
    },
    async waitFor(condition, timeoutMs = 5_000) {
      const deadline = Date.now() + timeoutMs;
      while (!condition()) {
        if (Date.now() > deadline) throw new Error("fake T3 condition not met in time");
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
    },
    async close() {
      fake.disconnectAll();
      wss.close();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
  return fake;

  async function handle(socket: WebSocket, frame: Record<string, unknown>): Promise<void> {
    if (frame["_tag"] === "Ping") {
      send(socket, { _tag: "Pong" });
      return;
    }
    if (frame["_tag"] === "Interrupt") {
      for (const thread of threads.values())
        for (const subscriber of thread.subscribers)
          if (subscriber.socket === socket && subscriber.requestId === frame["requestId"])
            thread.subscribers.delete(subscriber);
      return;
    }
    if (frame["_tag"] !== "Request") return;
    if (fake.malformed) {
      fake.malformed = false;
      socket.send("{not json");
      return;
    }
    const id = frame["id"];
    const payload = frame["payload"];
    switch (frame["tag"]) {
      case "orchestration.dispatchCommand": {
        if (!isCommand(payload)) {
          rejectRequest(socket, id, "OrchestrationV2DispatchCommandError", "invalid command");
          return;
        }
        received.push(payload);
        if (fake.dropRequest(payload)) {
          socket.terminate();
          return;
        }
        const delay = fake.delayAnswer(payload);
        if (delay > 0) await new Promise((resolve) => setTimeout(resolve, delay));
        const rejection = fake.reject(payload);
        if (rejection !== undefined) {
          rejectRequest(socket, id, "OrchestrationV2DispatchCommandError", rejection);
          return;
        }
        let sequence: number;
        try {
          sequence = dispatch(payload);
        } catch (failure) {
          rejectRequest(
            socket,
            id,
            "OrchestrationV2DispatchCommandError",
            failure instanceof Error ? failure.message : String(failure),
          );
          return;
        }
        if (fake.dropAnswer(payload)) {
          socket.terminate();
          return;
        }
        exit(socket, id, { _tag: "Success", value: { sequence } });
        return;
      }
      case "orchestration.subscribeThread": {
        const input = isRecord(payload) ? payload : {};
        const threadId = input["threadId"];
        const thread = typeof threadId === "string" ? threads.get(threadId) : undefined;
        if (thread === undefined) {
          rejectRequest(
            socket,
            id,
            "OrchestrationV2GetThreadProjectionError",
            `Failed to load orchestration V2 thread ${String(threadId)}`,
          );
          return;
        }
        const afterSequence =
          typeof input["afterSequence"] === "number" ? input["afterSequence"] : undefined;
        const subscription: Subscription = {
          afterSequence,
          bounded: input["acceptBoundedSnapshot"] === true,
          marker: input["requestCompletionMarker"] === true,
        };
        subscriptions.push(subscription);
        thread.subscribers.add({ socket, requestId: id });
        const replay =
          afterSequence !== undefined && !fake.snapshotOnResume && afterSequence <= thread.sequence
            ? thread.events.filter((event) => event.sequence > afterSequence)
            : undefined;
        const values: unknown[] =
          replay === undefined
            ? [
                {
                  kind: "snapshot",
                  snapshotSequence: thread.sequence,
                  ...(subscription.bounded
                    ? { historyCursor: null, hasMoreHistory: false, payloadBudgetExceeded: false }
                    : {}),
                  projection: {
                    thread: { id: thread.id, projectId: thread.projectId },
                    runs: thread.runs,
                    runtimeRequests: thread.requests,
                    providerThreads: [thread.providerThread],
                    providerSessions: [thread.session],
                    messages: thread.messages.map((message) => ({ ...message, streaming: false })),
                    turnItems: thread.items,
                  },
                },
              ]
            : replay.map((event) => streamEvent(thread, event));
        if (subscription.marker) values.push({ kind: "synchronized" });
        send(socket, { _tag: "Chunk", requestId: id, values });
        return;
      }
      default:
        rejectRequest(socket, id, "RpcNotFound", String(frame["tag"]));
    }
  }

  function dispatch(command: Command): number {
    const replay = receipts.get(command.commandId);
    commands.push(command);
    if (replay !== undefined) return replay;
    switch (command.type) {
      case "thread.create": {
        if (threads.has(command.threadId)) throw new Error(`thread ${command.threadId} exists`);
        const sessionId = `provider-session-${command.threadId}`;
        const thread: FakeThread = {
          id: command.threadId,
          projectId: String(command["projectId"]),
          runs: [],
          messages: [],
          items: [],
          requests: [],
          providerThread: {
            id: `provider-thread-${command.threadId}`,
            providerSessionId: sessionId,
          },
          session: { id: sessionId, status: "ready" },
          sequence: 1,
          events: [],
          subscribers: new Set(),
        };
        threads.set(thread.id, thread);
        receipts.set(command.commandId, thread.sequence);
        return thread.sequence;
      }
      case "message.dispatch": {
        const thread = threadOf(command.threadId);
        startRun(thread, String(command["messageId"]), String(command["text"]));
        receipts.set(command.commandId, thread.sequence);
        return thread.sequence;
      }
      case "run.interrupt": {
        const thread = threadOf(command.threadId);
        const runId = String(command["runId"]);
        const run = thread.runs.find((candidate) => candidate.id === runId);
        const latest = thread.runs.at(-1);
        const roster = thread.providerThread.pendingBackgroundTasks ?? [];
        if (run === undefined) throw new Error(`Run ${runId} is not interruptible.`);
        if (ACTIVE.has(run.status)) {
          interrupts.push(runId);
          if (fake.interruptBehaviour === "settle") updateRun(thread.id, runId, "interrupted");
        } else if (run.id === latest?.id && roster.length > 0) {
          // T3 settles a stopped thread's remaining background work through its latest run.
          interrupts.push(runId);
          if (fake.interruptBehaviour === "settle") settleBackground(thread);
        } else throw new Error(`Run ${runId} is not interruptible.`);
        receipts.set(command.commandId, thread.sequence);
        return thread.sequence;
      }
      default:
        throw new Error(`unsupported command ${command.type}`);
    }
  }

  function settleBackground(thread: FakeThread): void {
    const now = new Date().toISOString();
    thread.items.forEach((item, index) => {
      if (!BACKGROUND_ITEMS.has(item.type) || !RUNNING_ITEMS.has(item.status)) return;
      const interrupted = {
        ...item,
        status: "interrupted" as const,
        completedAt: now,
        updatedAt: now,
      };
      thread.items[index] = interrupted;
      emit(thread, "turn-item.updated", interrupted);
    });
    fake.background(thread.id, []);
  }

  function startRun(thread: FakeThread, messageId: string, text: string): Run {
    const active = thread.runs.some((run) => ACTIVE.has(run.status));
    const run: Run = {
      id: `run-${thread.id.slice(0, 8)}-${String(thread.runs.length + 1)}`,
      threadId: thread.id,
      ordinal: thread.runs.length + 1,
      userMessageId: messageId,
      providerThreadId: thread.providerThread.id,
      status: active ? "queued" : "running",
    };
    const user: Message = {
      id: messageId,
      runId: run.id,
      role: "user",
      text,
      createdAt: new Date().toISOString(),
    };
    thread.messages.push(user);
    thread.runs.push(run);
    emit(thread, "message.updated", { ...user, streaming: false });
    emit(thread, "run.created", run);
    return run;
  }

  function updateRun(threadId: string, runId: string, status: Run["status"]): void {
    const thread = threadOf(threadId);
    const index = thread.runs.findIndex((run) => run.id === runId);
    const run = thread.runs[index];
    if (run === undefined) throw new Error(`unknown run ${runId}`);
    const updated = { ...run, status };
    thread.runs[index] = updated;
    emit(thread, "run.updated", updated);
    if (ACTIVE.has(status) || fake.retainRequests) return;
    // A run that ends takes its open requests with it, as T3 does.
    thread.requests.forEach((request, position) => {
      if (request.status !== "pending" || requestRuns.get(request.id) !== runId) return;
      const cancelled = { ...request, status: "cancelled" as const };
      thread.requests[position] = cancelled;
      emit(thread, "runtime-request.updated", cancelled);
    });
    const next = thread.runs.find((candidate) => candidate.status === "queued");
    if (next !== undefined) updateRun(threadId, next.id, "running");
  }

  function emit(thread: FakeThread, type: string, payload: unknown): void {
    thread.sequence += 1;
    const event: StoredEvent = { sequence: thread.sequence, type, payload };
    thread.events.push(event);
    for (const subscriber of thread.subscribers)
      send(subscriber.socket, {
        _tag: "Chunk",
        requestId: subscriber.requestId,
        values: [streamEvent(thread, event)],
      });
  }

  function streamEvent(thread: FakeThread, event: StoredEvent): unknown {
    return {
      kind: "event",
      sequence: event.sequence,
      event: {
        id: `event-${String(event.sequence)}`,
        threadId: thread.id,
        type: event.type,
        payload: event.payload,
        occurredAt: new Date().toISOString(),
      },
    };
  }

  function threadOf(threadId: string): FakeThread {
    const thread = threads.get(threadId);
    if (thread === undefined) throw new Error(`unknown thread ${threadId}`);
    return thread;
  }

  function rejectRequest(socket: WebSocket, requestId: unknown, tag: string, detail: string): void {
    exit(socket, requestId, {
      _tag: "Failure",
      cause: [{ _tag: "Fail", error: { _tag: tag, message: `${tag} from the fake T3`, detail } }],
    });
  }

  function exit(socket: WebSocket, requestId: unknown, exitValue: unknown): void {
    send(socket, { _tag: "Exit", requestId, exit: exitValue });
  }

  function send(socket: WebSocket, frame: unknown): void {
    if (socket.readyState === socket.OPEN) socket.send(JSON.stringify(frame));
  }
}

function text(data: RawData): string {
  if (Array.isArray(data)) return Buffer.concat(data).toString("utf8");
  if (data instanceof ArrayBuffer) return Buffer.from(data).toString("utf8");
  return data.toString("utf8");
}

function isCommand(value: unknown): value is Command {
  return (
    isRecord(value) &&
    typeof value["type"] === "string" &&
    typeof value["commandId"] === "string" &&
    typeof value["threadId"] === "string"
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
