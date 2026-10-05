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
import { Check } from "typebox/value";
import { WebSocketServer, type RawData, type WebSocket } from "ws";
import {
  NativeSelectionSchema,
  ORCHESTRATION_PROTOCOL,
  type ContextTransfer,
  type Message,
  type NativeSelection,
  type ProviderSession,
  type ProviderThread,
  type Run,
  type RuntimeRequest,
  type Subagent,
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
  modelSelection: NativeSelection;
  readonly runs: Run[];
  readonly messages: Message[];
  readonly items: FakeItem[];
  readonly requests: RuntimeRequest[];
  readonly subagents: Subagent[];
  readonly transfers: ContextTransfer[];
  providerThread: ProviderThread;
  session: ProviderSession;
  /** T3 removed the session's record; the process closes later and may still deliver until it does. */
  sessionDetached: boolean;
  sessionClosed: boolean;
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
  /** Every provider session detached, by session. */
  readonly detached: string[];
  /** Every thread subscription received, in order. */
  readonly subscriptions: Subscription[];
  /** Every `server.refreshProviders` request, in order. */
  readonly refreshes: unknown[];
  /** Images by attachment ID, as granted and, once posted, with their bytes. */
  readonly uploads: Map<
    string,
    { readonly name: string; readonly mimeType: string; readonly sizeBytes: number; bytes?: Buffer }
  >;
  /** The upload address T3 grants for an attachment; a fault may name another origin. */
  uploadUrl: (attachmentId: string) => string;
  /** What the upload route answers a complete upload with. */
  uploadStatus: number;
  /** What protocol discovery announces. */
  protocolVersion: number;
  /** The provider instances `server.getConfig` and `server.refreshProviders` list, as T3 describes them. */
  providers: unknown[];
  /** Processes the command, then drops the socket before answering. */
  dropAnswer: (command: Command) => boolean;
  /** Drops the socket before processing the command. */
  dropRequest: (command: Command) => boolean;
  /** Delays the answer to a command, in milliseconds. */
  delayAnswer: (command: Command) => number;
  /** Rejects the command with this detail instead of processing it; T3 keeps a rejected receipt for it. */
  reject: (command: Command) => string | undefined;
  /** Processes the command, then answers with this failure instead of its receipt, as a post-commit failure does. */
  failAfterDispatch: (command: Command) => string | undefined;
  /** Processes the command, then answers with an internal defect (a `Die` exit). */
  defect: (command: Command) => boolean;
  /** Lists only the last so many messages in a snapshot, as T3's bounded window may. */
  messageWindow: number | undefined;
  /** Omits matching runs from snapshots: a broken snapshot. */
  omitRun: (run: Run) => boolean;
  /** Answers the next request with a frame that is not JSON. */
  malformed: boolean;
  /** Whether an interrupt stops the run (and settles background work) or is accepted and ignored. */
  interruptBehaviour: "settle" | "ignore";
  /** Answers a resume with a snapshot instead of a replay, as T3 does when the replay is too long. */
  snapshotOnResume: boolean;
  /** Leaves a run\'s pending requests open when it ends: a contradictory projection. */
  retainRequests: boolean;
  /** Omits request items from snapshots, as T3\'s bounded history window may. */
  omitRequestItems: boolean;
  /** Accepts subscriptions but never delivers their first snapshot or marker. */
  holdSnapshot: boolean;
  /** Omits matching child agents from snapshots, as a broken or skewed snapshot might. */
  omitSubagent: (task: Subagent) => boolean;
  /** Accepts protocol discovery requests but never answers them. */
  holdDiscovery: boolean;
  /** Answers protocol discovery with its headers but never sends the body. */
  holdDiscoveryBody: boolean;
  /** Accepts upgrade requests but never completes them. */
  holdUpgrade: boolean;
  /** Closes every client socket, as a server restart would. */
  disconnectAll(): void;
  tool(threadId: string, runId: string, item: Partial<FakeItem> & { type: string }): FakeItem;
  finishTool(threadId: string, item: FakeItem, status?: TurnItem["status"]): void;
  /** The run's assistant message, or a further one of the run when `part` names it. */
  assistant(
    threadId: string,
    runId: string,
    text: string,
    streaming?: boolean,
    part?: string,
  ): void;
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
  /**
   * The run delegates a task to a T3-owned child agent, as `delegate_task`
   * does: the task record names the run that started it.
   */
  delegate(
    threadId: string,
    runId: string,
    task: {
      readonly title?: string;
      readonly model?: string;
      readonly wake?: "always" | "settled_only";
    },
  ): Subagent;
  /** A child agent's own update, such as a provider-native subagent or a nested one. */
  subagent(threadId: string, task: Subagent): void;
  /**
   * The child finishes and T3 reports it to its parent as the serving build
   * does: the task's result is published with its delivery claimed, the
   * parent run announces the continuation message, then T3 starts the
   * continuation run (queued behind active work) and marks the delivery
   * delivered. Returns the continuation run.
   */
  finishDelegation(threadId: string, taskId: string, status?: Subagent["status"]): Run;
  /** A context transfer T3 records for the thread, such as a provider handoff. */
  transfer(threadId: string, transfer: ContextTransfer): void;
  session(threadId: string, status: ProviderSession["status"]): void;
  /** The detached process finally closes: nothing further can be delivered. */
  closeSession(threadId: string): void;
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

/** A catalogue shaped like the serving build's: one usable instance and one disabled. */
export const FAKE_PROVIDERS: readonly unknown[] = [
  {
    instanceId: "claudeAgent",
    driver: "claudeAgent",
    enabled: true,
    installed: true,
    status: "ready",
    models: [
      {
        slug: "claude-sonnet-5",
        name: "Claude Sonnet 5",
        isCustom: false,
        capabilities: {
          optionDescriptors: [
            {
              id: "effort",
              label: "Effort",
              type: "select",
              options: [
                { id: "low", label: "Low" },
                { id: "high", label: "High", isDefault: true },
              ],
            },
            { id: "fastMode", label: "Fast Mode", type: "boolean" },
          ],
        },
      },
      { slug: "claude-haiku-4-5", name: "Claude Haiku 4.5", isCustom: false, capabilities: null },
    ],
  },
  {
    instanceId: "cursor",
    driver: "cursor",
    enabled: false,
    installed: false,
    status: "disabled",
    models: [{ slug: "auto", name: "Auto", isCustom: false, capabilities: null }],
  },
];

export async function startFakeT3(token = "fake-t3-token"): Promise<FakeT3> {
  const threads = new Map<string, FakeThread>();
  const received: Command[] = [];
  const commands: Command[] = [];
  const interrupts: string[] = [];
  const detached: string[] = [];
  const subscriptions: Subscription[] = [];
  const refreshes: unknown[] = [];
  const uploads: FakeT3["uploads"] = new Map();
  const receipts = new Map<string, number>();
  const rejected = new Map<string, string>();
  const requestRuns = new Map<string, string>();
  const sockets = new Set<WebSocket>();

  /** Sockets of discovery and upgrade requests held open by a fault. */
  const held = new Set<Duplex>();
  const server = createServer((request, response) => {
    if (request.url?.startsWith("/ws") === true) {
      if (fake.holdDiscovery) {
        held.add(request.socket);
        return;
      }
      response.writeHead(426, { "content-type": "application/json" });
      if (fake.holdDiscoveryBody) {
        held.add(request.socket);
        response.flushHeaders();
        return;
      }
      response.end(
        JSON.stringify({
          code: "orchestration_protocol_incompatible",
          orchestrationProtocolVersion: fake.protocolVersion,
        }),
      );
      return;
    }
    const upload = /^\/api\/attachments\/upload\/([^.]+)\.signed$/u.exec(request.url ?? "");
    const granted = upload?.[1] === undefined ? undefined : uploads.get(upload[1]);
    if (request.method === "POST" && upload?.[1] !== undefined && granted !== undefined) {
      // As T3's route: the length must match the grant exactly.
      if (Number(request.headers["content-length"]) !== granted.sizeBytes) {
        response.writeHead(400);
        response.end("Content-Length must match the upload size.");
        return;
      }
      const chunks: Buffer[] = [];
      request.on("data", (chunk: Buffer) => chunks.push(chunk));
      request.on("end", () => {
        if (fake.uploadStatus === 204) granted.bytes = Buffer.concat(chunks);
        response.writeHead(fake.uploadStatus);
        response.end();
      });
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
    if (fake.holdUpgrade) {
      held.add(socket);
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
    detached,
    subscriptions,
    refreshes,
    uploads,
    uploadUrl: (attachmentId) => `/api/attachments/upload/${attachmentId}.signed`,
    uploadStatus: 204,
    protocolVersion: ORCHESTRATION_PROTOCOL,
    providers: [...FAKE_PROVIDERS],
    dropAnswer: () => false,
    dropRequest: () => false,
    delayAnswer: () => 0,
    reject: () => undefined,
    failAfterDispatch: () => undefined,
    defect: () => false,
    messageWindow: undefined,
    omitRun: () => false,
    malformed: false,
    interruptBehaviour: "settle",
    snapshotOnResume: false,
    retainRequests: false,
    omitRequestItems: false,
    holdSnapshot: false,
    omitSubagent: () => false,
    holdDiscovery: false,
    holdDiscoveryBody: false,
    holdUpgrade: false,
    disconnectAll() {
      for (const socket of sockets) socket.terminate();
      for (const socket of held) socket.destroy();
      held.clear();
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
    assistant(threadId, runId, text, streaming = false, part) {
      const thread = threadOf(threadId);
      const id = part === undefined ? `assistant-${runId}` : `assistant-${runId}-${part}`;
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
      if (thread.sessionClosed)
        throw new Error("the provider process has closed; no wake can come");
      return startRun(thread, `message:${thread.id}:wake:${String(thread.runs.length + 1)}`, text);
    },
    delegate(threadId, runId, task) {
      const thread = threadOf(threadId);
      const id = `node:delegated-task:${String(thread.subagents.length + 1)}:${thread.id}`;
      const record: Subagent = {
        id,
        runId,
        parentNodeId: `node:run:${runId}:root`,
        origin: "app_owned",
        driver: "claudeAgent",
        status: "running",
        title: task.title ?? null,
        model: task.model ?? null,
        childThreadId: `thread:delegated-task:${id}`,
        startedAt: new Date().toISOString(),
        completionWake: task.wake ?? "always",
      };
      fake.subagent(threadId, record);
      fake.tool(threadId, runId, { type: "subagent", status: "running", subagentId: id });
      return record;
    },
    subagent(threadId, task) {
      const thread = threadOf(threadId);
      const index = thread.subagents.findIndex((candidate) => candidate.id === task.id);
      if (index === -1) thread.subagents.push(task);
      else thread.subagents[index] = task;
      emit(thread, "subagent.updated", task);
    },
    finishDelegation(threadId, taskId, status = "completed") {
      const thread = threadOf(threadId);
      const task = thread.subagents.find((candidate) => candidate.id === taskId);
      if (task?.runId == null) throw new Error(`unknown delegated task ${taskId}`);
      const parentIndex = thread.runs.findIndex((run) => run.id === task.runId);
      const parent = thread.runs[parentIndex];
      if (parent === undefined) throw new Error(`unknown parent run ${task.runId}`);
      const messageId = `message:${thread.id}:continuation:${String(thread.runs.length + 1)}`;
      fake.subagent(threadId, { ...task, status, completionDelivery: { state: "claimed" } });
      const announcing = {
        ...parent,
        delegatedCompletion: { disposition: "open", delivery: { messageId } },
      };
      thread.runs[parentIndex] = announcing;
      emit(thread, "run.updated", announcing);
      fake.transfer(threadId, {
        id: `context-transfer:subagent_result:${taskId}`,
        type: "subagent_result",
        status: "consumed",
        sourceThreadId: task.childThreadId ?? taskId,
        targetThreadId: thread.id,
        sourcePoint: { threadId: task.childThreadId ?? taskId },
        targetRunId: null,
        targetProviderInstanceId: null,
      });
      const run = startRun(
        thread,
        messageId,
        `Delegated task ${taskId} reached a terminal state.`,
        { parentRunId: parent.id },
      );
      fake.subagent(threadId, {
        ...task,
        status,
        completionDelivery: { state: "delivered" },
      });
      const settledParent = {
        ...announcing,
        delegatedCompletion: { disposition: "open", delivery: null },
      };
      thread.runs[parentIndex] = settledParent;
      emit(thread, "run.updated", settledParent);
      return run;
    },
    transfer(threadId, transfer) {
      const thread = threadOf(threadId);
      const index = thread.transfers.findIndex((candidate) => candidate.id === transfer.id);
      if (index === -1) thread.transfers.push(transfer);
      else thread.transfers[index] = transfer;
      emit(
        thread,
        index === -1 ? "context-transfer.created" : "context-transfer.updated",
        transfer,
      );
    },
    session(threadId, status) {
      const thread = threadOf(threadId);
      thread.session = { ...thread.session, status };
      if (!thread.sessionDetached) emit(thread, "provider-session.updated", thread.session);
    },
    closeSession(threadId) {
      const thread = threadOf(threadId);
      thread.sessionClosed = true;
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
        // As T3 does: a command that committed nothing keeps a rejected
        // receipt, and its replay answers with that receipt.
        const previous = rejected.get(payload.commandId);
        if (previous !== undefined) {
          rejectRequest(
            socket,
            id,
            "OrchestratorCommandPreviouslyRejectedError",
            `Command ${payload.commandId} was previously rejected: ${previous}`,
          );
          return;
        }
        const rejection = fake.reject(payload);
        if (rejection !== undefined) {
          rejected.set(payload.commandId, rejection);
          rejectRequest(socket, id, "OrchestrationV2DispatchCommandError", rejection);
          return;
        }
        let sequence: number;
        try {
          sequence = dispatch(payload);
        } catch (failure) {
          const detail = failure instanceof Error ? failure.message : String(failure);
          rejected.set(payload.commandId, detail);
          rejectRequest(socket, id, "OrchestrationV2DispatchCommandError", detail);
          return;
        }
        if (fake.dropAnswer(payload)) {
          socket.terminate();
          return;
        }
        const late = fake.failAfterDispatch(payload);
        if (late !== undefined) {
          rejectRequest(socket, id, "OrchestratorDispatchError", late);
          return;
        }
        if (fake.defect(payload)) {
          exit(socket, id, {
            _tag: "Failure",
            cause: [{ _tag: "Die", defect: { message: "internal defect from the fake T3" } }],
          });
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
            `Failed to load orchestration V2 thread ${String(threadId)}: No orchestration projection exists for thread ${String(threadId)}.`,
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
        if (fake.holdSnapshot) return;
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
                    thread: {
                      id: thread.id,
                      projectId: thread.projectId,
                      modelSelection: thread.modelSelection,
                    },
                    runs: thread.runs.filter((run) => !fake.omitRun(run)),
                    subagents: thread.subagents.filter((task) => !fake.omitSubagent(task)),
                    contextTransfers: thread.transfers,
                    runtimeRequests: thread.requests,
                    providerThreads: [thread.providerThread],
                    providerSessions: thread.sessionDetached ? [] : [thread.session],
                    messages: thread.messages
                      .slice(fake.messageWindow === undefined ? 0 : -fake.messageWindow)
                      .map((message) => ({ ...message, streaming: false })),
                    turnItems: fake.omitRequestItems
                      ? thread.items.filter((item) => !item.type.endsWith("_request"))
                      : thread.items,
                  },
                },
              ]
            : replay.map((event) => streamEvent(thread, event));
        if (subscription.marker) values.push({ kind: "synchronized" });
        send(socket, { _tag: "Chunk", requestId: id, values });
        return;
      }
      case "server.getConfig":
        exit(socket, id, { _tag: "Success", value: { providers: fake.providers } });
        return;
      case "attachments.createUploadUrl": {
        const input = isRecord(payload) ? payload : {};
        const { name, mimeType, sizeBytes } = input;
        if (
          input["type"] !== "image" ||
          typeof name !== "string" ||
          typeof mimeType !== "string" ||
          typeof sizeBytes !== "number" ||
          sizeBytes > 10 * 1024 * 1024
        ) {
          rejectRequest(socket, id, "SchemaError", "invalid upload request");
          return;
        }
        const attachmentId = `pending-${String(uploads.size + 1)}`;
        uploads.set(attachmentId, { name, mimeType, sizeBytes });
        exit(socket, id, {
          _tag: "Success",
          value: {
            attachmentId,
            relativeUrl: fake.uploadUrl(attachmentId),
            expiresAt: Date.now() + 600_000,
          },
        });
        return;
      }
      case "server.refreshProviders":
        refreshes.push(payload);
        exit(socket, id, { _tag: "Success", value: { providers: fake.providers } });
        return;
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
          modelSelection: selectionOf(command),
          runs: [],
          messages: [],
          items: [],
          requests: [],
          subagents: [],
          transfers: [],
          providerThread: {
            id: `provider-thread-${command.threadId}`,
            providerSessionId: sessionId,
          },
          session: { id: sessionId, status: "ready" },
          sessionDetached: false,
          sessionClosed: false,
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
      case "thread.model-selection.set": {
        const thread = threadOf(command.threadId);
        thread.modelSelection = selectionOf(command);
        emit(thread, "thread.model-selection-updated", {
          id: thread.id,
          modelSelection: thread.modelSelection,
        });
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
      case "provider-session.detach": {
        const thread = threadOf(command.threadId);
        const sessionId = String(command["providerSessionId"]);
        if (thread.session.id !== sessionId || thread.sessionDetached)
          throw new Error(`Provider session ${sessionId} does not belong to thread ${thread.id}.`);
        detached.push(sessionId);
        // As T3 does: the record goes now; the process closes later, if at all.
        if (fake.interruptBehaviour === "settle") {
          thread.sessionDetached = true;
          emit(thread, "provider-session.detached", { providerSessionId: sessionId });
        }
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

  function startRun(
    thread: FakeThread,
    messageId: string,
    text: string,
    delegatedCompletion?: { readonly parentRunId: string },
  ): Run {
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
      ...(delegatedCompletion === undefined ? {} : { delegatedCompletion }),
    };
    thread.messages.push(user);
    thread.runs.push(run);
    // The serving order: the dispatch sends the provider thread before the
    // run, and the turn start sends the session, then the thread, then the
    // run. A continuation's message, with its link, follows its run.
    if (delegatedCompletion === undefined)
      emit(thread, "message.updated", { ...user, streaming: false });
    emit(thread, "provider-thread.updated", thread.providerThread);
    emit(thread, "run.created", run);
    if (delegatedCompletion !== undefined)
      emit(thread, "message.updated", { ...user, streaming: false });
    if (!thread.sessionDetached)
      emit(thread, "provider-session.updated", { ...thread.session, status: "running" });
    emit(thread, "provider-thread.updated", thread.providerThread);
    emit(thread, "run.updated", run);
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

/** The command's model selection, refused as T3's validator would when malformed. */
function selectionOf(command: Command): NativeSelection {
  const selection = command["modelSelection"];
  if (!Check(NativeSelectionSchema, selection)) throw new Error("invalid model selection");
  return selection;
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
