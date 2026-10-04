import type {
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
}

export interface RuntimeOperations {
  executeTurn(request: TurnRequest, signal?: AbortSignal): Promise<TurnResponse>;
  /** The latest activity of a turn this instance is executing for that interface. */
  activity(correlationId: string, interfaceId: string): TurnActivity | undefined;
  modelState(query: ModelQuery): Promise<ModelState>;
  setModel(update: ModelUpdate): Promise<ModelState>;
  close(): Promise<void>;
}
