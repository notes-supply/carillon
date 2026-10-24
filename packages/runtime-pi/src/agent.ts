/**
 * One agent on pi-durable: its harness, the turns core hands over, and the
 * controls people use on them. Each turn's message is submitted to its
 * conversation's inbox, and a delivery task settles the turn with core.
 */
import { copyJson } from "@earendil-works/chord";
import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
import type { Api, Model } from "@earendil-works/pi-ai";
import {
  AgentDoc,
  configure,
  createRegistry,
  Harness,
  InboxDoc,
  LiveDoc,
  type ConversationId,
  type Extension,
  type Storage,
  type SubmissionId,
  type TaskId,
  type TaskInspection,
  type UserInput,
} from "@earendil-works/pi-durable";
import { NodeExecutionEnv } from "@earendil-works/pi-durable/env/node";
import { DEFAULT_IMAGE_LIMITS, type ImageProcessor } from "@earendil-works/pi-durable/tools";
import type { ControlAction, TurnActivity } from "@carillon/protocol";
import type { CoreClient } from "@carillon/runtime/core";
import { diagnostic, ExplainedFailure, RuntimeClosed } from "@carillon/runtime/failure";
import type { TurnRequest } from "@carillon/runtime/operations";
import { resolve } from "node:path";
import { watchActivity, type ConversationActivity } from "./activity.ts";
import { prepareAttachments, type InlineImagePolicy, type ModelImages } from "./attachments.ts";
import {
  CRASHED,
  DELIVERY,
  deliveryTask,
  isDeliveryInput,
  type DeliveryInput,
} from "./delivery.ts";
import {
  ConversationInfo,
  conversationKey,
  MESSAGE,
  Resources,
  Sessions,
  Turns,
} from "./documents.ts";
import type { Catalogue, ModelChoice, ModelChoices } from "./models.ts";
import {
  carillonExtension,
  isUserExtension,
  loadSnapshot,
  selectedExtensions,
  type Extensions,
  type Snapshot,
} from "./resources.ts";

const context = BACKGROUND_CONTEXT;
const INTERRUPTED = "[Carillon] This message interrupted your previous answer.";
/** The most times a run is aborted again because its task handed over meanwhile. */
const ABORT_ATTEMPTS = 5;

export interface AgentOptions {
  readonly id: string;
  readonly workspace: string;
  readonly storage: Storage;
  readonly catalogue: Catalogue;
  readonly choices: ModelChoices;
  readonly core: CoreClient;
  readonly images: ImageProcessor;
  readonly inlineImages: InlineImagePolicy;
  /** The built-in extensions, named as settings select them. */
  readonly builtIns: readonly Extension[];
  /** What settings select when they name no extensions. */
  readonly defaults: readonly string[];
}

export interface DurableAgent {
  submit(request: TurnRequest): Promise<void>;
  /** Whether core handed this agent the turn, settled or not. */
  knows(turnId: string): Promise<boolean>;
  control(turnId: string, interfaceId: string, action: ControlAction): Promise<void>;
  activity(turnId: string, interfaceId: string): TurnActivity | undefined;
  /** Whether one of the agent's tool calls runs now. */
  readonly calling: boolean;
  close(): Promise<void>;
}

export async function openAgent(options: AgentOptions): Promise<DurableAgent> {
  const workspace = resolve(options.workspace);
  const registry = createRegistry();
  /** Turns the agent holds whose outcome core has not recorded, by turn ID. */
  const held = new Map<string, DeliveryInput>();
  /** Admissions under way, by turn ID: core handing a turn over again joins the first. */
  const admitting = new Map<string, Promise<void>>();
  /** Each conversation's controls and background interrupts, run in the order they were asked for. */
  const queues = new Map<ConversationId, Promise<void>>();
  let closing = false;
  const watches = new Map<ConversationId, Promise<ConversationActivity>>();
  const watched = new Map<ConversationId, ConversationActivity>();
  let latest: Snapshot | undefined;
  /** Whether adopting the latest snapshot kept an old user extension or left a module out. */
  let incomplete = false;

  const delivery = deliveryTask({
    wait: async (submission, waiting) => {
      const handle = await harness.submission(submission, waiting);
      if (handle === undefined) throw new Error(`submission ${submission} does not exist`);
      return handle.wait(waiting);
    },
    report: (turn) => {
      const input = held.get(turn);
      return input === undefined
        ? undefined
        : watched.get(input.conversation)?.of(input.submission);
    },
    settle: (turn, outcome) => options.core.settle(turn, outcome),
    released: release,
  });
  const extensions: Extensions = {
    carillon: carillonExtension(() => latest, workspace, [delivery]),
    builtIn: options.builtIns,
    defaults: options.defaults,
  };
  for (const extension of [extensions.carillon, ...extensions.builtIn]) registry.install(extension);

  const harness = await Harness.open(
    options.storage,
    {
      models: options.catalogue.models,
      registry,
      env: () => new NodeExecutionEnv({ cwd: workspace }),
      settings: () => ({
        steeringMode: "all",
        followUpMode: "one-at-a-time",
        afterFailedRun: "continue",
        extensions: selectedExtensions(latest, options.defaults, (name) =>
          registry.snapshot().extension(name),
        ),
      }),
      onReport: (error, info) => {
        console.error(`agent ${options.id}: ${info.type}: ${diagnostic(error)}`);
      },
    },
    context,
  );

  // Nothing runs before resume, so every live delivery is held before it can
  // finish, and the latest snapshot's extensions are installed first. One
  // that no longer loads leaves the defaults until the next is admitted.
  const recorded = (await harness.snapshot(Resources, context))?.root;
  if (recorded !== undefined)
    try {
      await adopt(await loadSnapshot(recorded, workspace, extensions));
    } catch (failure) {
      console.error(
        `agent ${options.id}: resources ${recorded}: ${diagnostic(failure)}; using the default extensions`,
      );
    }
  const inspection = await harness.inspect(context);
  for (const { record } of inspection.tasks)
    if (record.kind === DELIVERY && isDeliveryInput(record.input)) hold(record.input);
  const given = await recover(inspection.tasks);
  harness.resume();
  for (const run of given) await harness.waitForTask(run, context);
  const waiting = new Set(
    inspection.submissions.flatMap((record) =>
      record.status === "queued" ? [record.conversationId] : [],
    ),
  );
  for (const conversation of waiting) await ensureFlowing(conversation);

  return {
    submit(request) {
      const pending = admitting.get(request.correlation_id);
      if (pending !== undefined) return pending;
      const admission = admit(request).finally(() => admitting.delete(request.correlation_id));
      admitting.set(request.correlation_id, admission);
      return admission;
    },

    async knows(turnId) {
      return held.has(turnId) || (await harness.snapshot(Turns, turnId, context)) !== undefined;
    },

    async control(turnId, interfaceId, action) {
      const input = held.get(turnId);
      if (input === undefined)
        throw new ExplainedFailure(409, "That message was already answered.");
      // A message is only ever controlled from the interface it came from.
      if (input.interface_id !== interfaceId)
        throw new ExplainedFailure(404, "This runtime holds no such message.");
      const { conversation, submission } = input;
      return inOrder(conversation, async () => {
        switch (action) {
          case "steer":
            if (!(await toSteers(conversation, submission)))
              throw new ExplainedFailure(409, "That message is no longer waiting.");
            await ensureFlowing(conversation);
            return;
          case "interrupt":
            if (!(await waitingMessages(conversation)).includes(submission))
              throw new ExplainedFailure(409, "That message is no longer waiting.");
            await interrupt(
              conversation,
              submission,
              "[Carillon] The person interrupted your previous answer.",
            );
            return;
          case "stop":
            switch (await harness.abortSubmission(submission, context, conversation)) {
              case "aborted":
                return;
              case "already_placed":
                if (!(await abortRun(conversation, submission)))
                  throw new ExplainedFailure(409, "That message was already answered.");
                if ((await waitingMessages(conversation)).length > 0)
                  await kick(conversation, "[Carillon] The person stopped your previous answer.");
                return;
              case "settled":
              case "not_found":
                throw new ExplainedFailure(409, "That message was already answered.");
            }
        }
      });
    },

    activity(turnId, interfaceId) {
      const input = held.get(turnId);
      if (input?.interface_id !== interfaceId) return undefined;
      return watched.get(input.conversation)?.of(input.submission);
    },

    get calling() {
      return [...watched.values()].some((activity) => activity.calling);
    },

    async close() {
      closing = true;
      await Promise.all(queues.values());
      await Promise.allSettled(
        [...watches.values()].map(async (started) => (await started).stop()),
      );
      await harness.close(context);
    },
  };

  async function admit(request: TurnRequest): Promise<void> {
    if ((await harness.snapshot(Turns, request.correlation_id, context))?.task !== undefined)
      return;
    const snapshot =
      latest?.root === resolve(request.resource_root)
        ? latest
        : await loadSnapshot(resolve(request.resource_root), workspace, extensions);
    const choice = await options.choices.forTurn(request);
    const model =
      choice.model === null
        ? undefined
        : options.catalogue.models.getModel(choice.model.provider, choice.model.modelId);
    const attachments = await prepareAttachments(
      request.attachment_root,
      request.attachments ?? [],
      options.inlineImages,
      modelImages(model),
    );
    const { conversation, admitted } = await harness.commit(async (tx) => {
      const session = await tx.doc(
        Sessions,
        conversationKey(request.interface_id, request.audience_id),
        null,
      );
      const known = session.conversation;
      const submitted =
        known !== undefined &&
        (await tx.submissionByRequest(known, request.correlation_id)) !== undefined;
      const id = known ?? (await tx.createConversation({ ownership: { kind: "ownerless" } })).id;
      if (known === undefined) session.conversation = id;
      if (submitted) return { conversation: id, admitted: true };
      const info = await tx.doc(ConversationInfo, id);
      info.interface_id = request.interface_id;
      info.agent_id = request.agent_id;
      info.audience_id = request.audience_id;
      if (request.native !== undefined) info.native = copyJson(request.native);
      if (attachments.note !== undefined) info.attachment_root = request.attachment_root;
      if (!runs(await tx.doc(AgentDoc, id), choice)) await configure(tx, id, choice);
      (await tx.doc(Resources)).root = snapshot.root;
      return { conversation: id, admitted: false };
    }, context);
    if (!admitted && (snapshot !== latest || incomplete)) await adopt(snapshot);
    const parts: Exclude<UserInput, string> = [
      ...(request.input.length === 0 ? [] : [{ type: "text" as const, text: request.input }]),
      ...(attachments.note === undefined
        ? []
        : [{ type: "text" as const, text: attachments.note }]),
      ...attachments.images,
    ];
    const [only] = parts;
    const submission = await (
      await handle(conversation)
    ).submit(
      {
        type: "input",
        content: parts.length === 1 && only?.type === "text" ? only.text : parts,
        requestId: request.correlation_id,
        // No boundary of a run places a follow-up into it, so a message
        // interrupting it cannot land in it before the abort.
        whenBusy: snapshot.busy === "steer" ? "steer" : "followUp",
        entry: { kind: MESSAGE, data: { turn: request.correlation_id } },
      },
      context,
    );
    const input: DeliveryInput = {
      turn: request.correlation_id,
      interface_id: request.interface_id,
      conversation,
      submission: submission.id,
    };
    await harness.commit(async (tx) => {
      const turn = await tx.doc(Turns, request.correlation_id, null);
      turn.task ??= await tx.createTask(delivery, input, {
        ownership: { kind: "conversation" },
        conversationId: conversation,
        background: true,
      });
    }, context);
    hold(input);
    // In interrupt mode, once handed over and after the conversation's
    // earlier controls, a message still waiting ends the run it waits behind.
    if (!admitted && snapshot.busy === "interrupt")
      inOrder(conversation, async () => {
        if ((await waitingMessages(conversation)).includes(submission.id))
          await interrupt(conversation, submission.id, INTERRUPTED);
      }).catch((failure: unknown) => {
        console.error(`agent ${options.id}: conversation ${conversation}: ${diagnostic(failure)}`);
      });
  }

  /**
   * Startup: parked deliveries run again, and a run that crashed the
   * process too often is given up, its messages failed. Returns the runs
   * given up, whose aborts finish once tasks run.
   */
  async function recover(tasks: readonly TaskInspection[]): Promise<TaskId[]> {
    const crashed = new Set<ConversationId>();
    for (const { record, state } of tasks) {
      if (state.kind !== "blocked" || state.reason !== "restart_limit") continue;
      if (record.kind === DELIVERY) await harness.retryTask(record.id, context);
      else if (record.kind === "pi.generation" || record.kind === "pi.tool")
        crashed.add(record.conversationId);
    }
    const given: TaskId[] = [];
    for (const conversation of crashed) {
      const run = await harness.commit(async (tx) => {
        const live = await tx.doc(LiveDoc, conversation);
        for (const input of live.run?.inputs ?? [])
          tx.settleSubmission(input, { status: "unanswered", reason: CRASHED });
        return live.run?.taskId;
      }, context);
      if (run === undefined) continue;
      await harness.abortTask(run, context);
      given.push(run);
    }
    return given;
  }

  /**
   * The snapshot becomes the agent's latest, its user extensions replacing
   * every installed one, as in a fresh registry. An old extension stays
   * while a live task needs one of its task names that no installed module
   * defines, and the next admission adopts the snapshot again. Once modules
   * define them all, they take the tasks over.
   */
  async function adopt(snapshot: Snapshot): Promise<void> {
    const live = new Set((await harness.inspect(context)).tasks.map(({ record }) => record.kind));
    // No await from here, so no task sees the registry part way.
    const kept = new Map<Extension, string[]>();
    for (const extension of registry.snapshot().installed()) {
      if (!isUserExtension(extension.name)) continue;
      const needed = taskNames(extension).filter((name) => live.has(name));
      if (needed.length > 0) kept.set(extension, needed);
      else registry.uninstall(extension);
    }
    let waiting = snapshot.modules.filter((module) => !install(module));
    for (let released = true; released;) {
      released = false;
      for (const [extension, needed] of kept) {
        registry.uninstall(extension);
        const installed = waiting.filter(install);
        if (needed.every((name) => registry.snapshot().task(name) !== undefined)) {
          kept.delete(extension);
          waiting = waiting.filter((module) => !installed.includes(module));
          released = true;
        } else {
          for (const module of installed) registry.uninstall(module);
          registry.install(extension);
        }
      }
    }
    if (snapshot !== latest) {
      for (const extension of kept.keys())
        console.error(`agent ${options.id}: keeping ${extension.name} while its tasks run`);
      for (const module of waiting)
        console.error(
          `agent ${options.id}: extension ${module.name} waits for the extensions kept for their tasks`,
        );
    }
    latest = snapshot;
    incomplete = kept.size > 0 || waiting.length > 0;
  }

  /**
   * Installs a user extension; false when one kept for its tasks has its
   * name or a task name, the only way a module of a snapshot fails here.
   */
  function install(module: Extension): boolean {
    if (registry.snapshot().extension(module.name) !== undefined) return false;
    try {
      registry.install(module);
      return true;
    } catch {
      return false;
    }
  }

  function hold(input: DeliveryInput): void {
    held.set(input.turn, input);
    if (watches.has(input.conversation)) return;
    const started = watchActivity(
      harness,
      input.conversation,
      workspace,
      options.catalogue.models,
      context,
    );
    watches.set(input.conversation, started);
    void started.then(
      (activity) => {
        if (watches.get(input.conversation) === started) watched.set(input.conversation, activity);
        else void activity.stop();
      },
      (failure: unknown) => {
        console.error(
          `agent ${options.id}: activity of ${input.conversation}: ${diagnostic(failure)}`,
        );
      },
    );
  }

  function release(turn: string): void {
    const input = held.get(turn);
    if (input === undefined) return;
    held.delete(turn);
    watched.get(input.conversation)?.forget(input.submission);
    if ([...held.values()].some((other) => other.conversation === input.conversation)) return;
    const started = watches.get(input.conversation);
    watches.delete(input.conversation);
    watched.delete(input.conversation);
    void started?.then((activity) => activity.stop()).catch(() => undefined);
  }

  async function handle(conversation: ConversationId) {
    const found = await harness.conversation(conversation, context);
    if (found === undefined) throw new Error(`conversation ${conversation} does not exist`);
    return found;
  }

  /** Runs work for the conversation after what was asked for before it. */
  function inOrder<T>(conversation: ConversationId, work: () => Promise<T>): Promise<T> {
    if (closing) return Promise.reject(new RuntimeClosed(`agent ${options.id} is closing`));
    const result = (queues.get(conversation) ?? Promise.resolve()).then(work);
    const done = result.then(
      () => undefined,
      () => undefined,
    );
    queues.set(conversation, done);
    void done.then(() => {
      if (queues.get(conversation) === done) queues.delete(conversation);
    });
    return result;
  }

  /**
   * Ends the conversation's run, unless it took `spared` meanwhile, then
   * runs every waiting message next: the follow-ups become steers, so the
   * note's write places them all in one run. With no run ended, waiting
   * messages only flow.
   */
  async function interrupt(
    conversation: ConversationId,
    spared: SubmissionId,
    note: string,
  ): Promise<void> {
    const run = (await harness.snapshot(LiveDoc, conversation, context))?.run?.inputs[0];
    const ended = run !== undefined && (await abortRun(conversation, run, spared));
    await toSteers(conversation);
    if (ended && (await waitingMessages(conversation)).length > 0) await kick(conversation, note);
    else await ensureFlowing(conversation);
  }

  /** Turns waiting follow-ups into steers: only `submission`, or every one; false when it no longer waits. */
  function toSteers(conversation: ConversationId, submission?: SubmissionId): Promise<boolean> {
    return harness.commit(async (tx) => {
      const { items } = await tx.doc(InboxDoc, conversation);
      if (
        submission !== undefined &&
        !items.some((item) => item.id === submission && item.mode !== "write")
      )
        return false;
      for (const item of items)
        if (item.mode === "followUp" && (submission === undefined || item.id === submission))
          item.mode = "steer";
      return true;
    }, context);
  }

  /**
   * Aborts the run holding `held`, unless it holds `spared`, and waits for
   * it to end; true when it was aborted. Its inputs settle as stopped;
   * queued messages stay, and nothing places them until the next submission
   * or write. The run's task changes as one generation hands over to the
   * next, so it is aborted again while the same run goes on, never a run
   * that followed it.
   */
  async function abortRun(
    conversation: ConversationId,
    held: SubmissionId,
    spared?: SubmissionId,
  ): Promise<boolean> {
    let aborted = false;
    for (let attempt = 0; attempt < ABORT_ATTEMPTS; attempt += 1) {
      const run = (await harness.snapshot(LiveDoc, conversation, context))?.run;
      if (run === undefined || !run.inputs.includes(held)) return aborted;
      if (spared !== undefined && run.inputs.includes(spared)) return aborted;
      if ((await harness.abortTask(run.taskId, context)) === "marked") aborted = true;
      await harness.waitForTask(run.taskId, context);
    }
    return aborted;
  }

  /** The messages waiting in the conversation's inbox, not its writes. */
  async function waitingMessages(conversation: ConversationId): Promise<SubmissionId[]> {
    const inbox = await harness.snapshot(InboxDoc, conversation, context);
    return inbox?.items.flatMap((item) => (item.mode === "write" ? [] : [item.id])) ?? [];
  }

  /** A model-visible note at the next boundary, which places the waiting messages with it. */
  async function kick(conversation: ConversationId, note: string): Promise<void> {
    await (
      await handle(conversation)
    ).submit(
      {
        type: "write",
        entry: {
          kind: "carillon.note",
          model: [{ role: "user", content: note, timestamp: Date.now() }],
        },
      },
      context,
    );
  }

  /** Starts a run for messages left waiting in an idle conversation, as after an abort. */
  async function ensureFlowing(conversation: ConversationId): Promise<void> {
    const live = await harness.snapshot(LiveDoc, conversation, context);
    if (live?.run !== undefined || (await waitingMessages(conversation)).length === 0) return;
    await (
      await handle(conversation)
    ).submit({ type: "write", entry: { kind: "carillon.flow", model: [] } }, context);
  }

  /** How the turn's model takes attached images. */
  function modelImages(model: Model<Api> | undefined): ModelImages {
    if (model !== undefined && !model.input.includes("image")) return { kind: "none" };
    const resize = model?.inputLimits?.images?.resize;
    return {
      kind: "inline",
      processor: options.images,
      limits: {
        maxWidth: resize?.maxWidth ?? DEFAULT_IMAGE_LIMITS.maxWidth,
        maxHeight: resize?.maxHeight ?? DEFAULT_IMAGE_LIMITS.maxHeight,
        maxBytes: resize?.maxBytes ?? DEFAULT_IMAGE_LIMITS.maxBytes,
      },
    };
  }
}

/** Whether the conversation's stored agent already runs the choice. */
function runs(
  agent: {
    readonly model?: { provider: string; modelId: string };
    readonly thinkingLevel?: string;
  },
  choice: ModelChoice,
): boolean {
  return (
    agent.model?.provider === choice.model?.provider &&
    agent.model?.modelId === choice.model?.modelId &&
    (agent.thinkingLevel ?? null) === choice.thinkingLevel
  );
}

function taskNames(extension: Extension): string[] {
  return (extension.tasks ?? []).map(({ definition }) => definition.name);
}
