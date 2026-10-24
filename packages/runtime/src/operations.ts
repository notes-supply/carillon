import {
  turnIntent,
  type CommandCatalog,
  type ControlAction,
  type ModelQuery,
  type ModelUpdate,
  type ModelState,
  type RuntimeTurnRequest,
  type TurnActivity,
} from "@carillon/protocol";

export type TurnRequest = RuntimeTurnRequest;

/**
 * The answer to a command invocation the engine ran to completion without
 * a reply of its own, such as a compaction: a short note naming it.
 * Undefined for any other turn, which has failed to answer.
 */
export function commandFinished(request: TurnRequest): string | undefined {
  if (turnIntent(request.native) !== "command") return undefined;
  const [name = ""] = request.input.trim().split(/\s+/u);
  return `\`${name.replaceAll("`", "")}\` finished.`;
}

export interface TurnResponse {
  readonly correlation_id: string;
  readonly output: string;
  /**
   * Native work linked to this turn will continue the conversation after
   * this answer: core queues a follow-up turn for the same conversation
   * (an observation) whose request carries this runtime-owned cursor as `follow_up`.
   */
  readonly follow_up?: unknown;
  /**
   * The answer left the earlier turns' files the request offered as
   * `unanswered` out of the native conversation, as a command passed to the
   * engine as written may: core offers them again with a later turn.
   */
  readonly unanswered_left?: boolean;
  /**
   * The turn's final report of what it did, taken before the runtime
   * discards its executing state, bounded by the shared `finalReport`.
   */
  readonly activity?: TurnActivity;
  /**
   * The answer left this turn's own files out of the native conversation, as
   * a command passed to the engine as written may when it cannot carry them:
   * core offers them again with a later turn.
   */
  readonly attachments_left?: boolean;
}

/**
 * Whether restarting the runtime now would lose something only its process
 * knows. Core asks before maintenance stops the services, and counts no
 * answer as not ready.
 */
export type RestartReadiness =
  | { readonly state: "ready" }
  | { readonly state: "unready"; readonly reason: string };

/** Runs each turn to its answer while core waits: T3's contract until it holds its turns. */
export interface AnswerTurns {
  readonly mode: "answer";
  execute(request: TurnRequest, signal?: AbortSignal): Promise<TurnResponse>;
}

/**
 * Keeps each turn core hands over and settles it later through core's
 * client. `submit` resolves once the turn is kept, and the same correlation
 * ID again is the same turn. `control` acts on a turn the runtime holds for
 * that interface, as a person asked.
 */
export interface InboxTurns {
  readonly mode: "inbox";
  submit(request: TurnRequest): Promise<void>;
  control?(turnId: string, interfaceId: string, action: ControlAction): Promise<void>;
}

export type TurnHandling = AnswerTurns | InboxTurns;

/** A runtime that answers each turn while core waits. */
export type AnswerRuntime = RuntimeOperations<AnswerTurns>;

export interface RuntimeOperations<Turns extends TurnHandling = TurnHandling> {
  readonly turns: Turns;
  /** The latest activity of a turn this instance is executing for that interface. */
  activity(correlationId: string, interfaceId: string): TurnActivity | undefined;
  modelState(query: ModelQuery): Promise<ModelState>;
  setModel(update: ModelUpdate): Promise<ModelState>;
  /** The engine's own commands a person may run in the agent's conversation, as the engine lists them now. */
  commands(query: ModelQuery): Promise<CommandCatalog>;
  restartReadiness(): RestartReadiness;
  close(): Promise<void>;
}
