import {
  turnIntent,
  type CommandCatalog,
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

export interface RuntimeOperations {
  executeTurn(request: TurnRequest, signal?: AbortSignal): Promise<TurnResponse>;
  /** The latest activity of a turn this instance is executing for that interface. */
  activity(correlationId: string, interfaceId: string): TurnActivity | undefined;
  modelState(query: ModelQuery): Promise<ModelState>;
  setModel(update: ModelUpdate): Promise<ModelState>;
  /** The engine's own commands a person may run in the agent's conversation, as the engine lists them now. */
  commands(query: ModelQuery): Promise<CommandCatalog>;
  close(): Promise<void>;
}
