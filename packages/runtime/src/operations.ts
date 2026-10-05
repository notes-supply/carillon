import type {
  CommandCatalog,
  ModelQuery,
  ModelUpdate,
  ModelState,
  RuntimeTurnRequest,
  TurnActivity,
} from "@carillon/protocol";

export type TurnRequest = RuntimeTurnRequest;

export interface TurnResponse {
  readonly correlation_id: string;
  readonly output: string;
  /**
   * Native work linked to this turn will continue the conversation after
   * this answer: core queues a follow-up turn for the same conversation
   * (an observation) whose request carries this runtime-owned cursor as `follow_up`.
   */
  readonly follow_up?: unknown;
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
