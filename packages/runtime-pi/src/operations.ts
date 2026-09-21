import type { ModelQuery, ModelUpdate, ModelState } from "@carillon/protocol";

export interface TurnRequest {
  readonly correlation_id: string;
  readonly interface_id: string;
  readonly agent_id: string;
  readonly audience_id: string;
  readonly input: string;
  readonly resource_root: string;
}

export interface TurnResponse {
  readonly correlation_id: string;
  readonly output: string;
}

export interface RuntimeOperations {
  executeTurn(request: TurnRequest, signal?: AbortSignal): Promise<TurnResponse>;
  modelState(query: ModelQuery): Promise<ModelState>;
  setModel(update: ModelUpdate): Promise<ModelState>;
  close(): Promise<void>;
}
