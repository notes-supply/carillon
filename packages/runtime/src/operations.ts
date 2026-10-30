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

/**
 * Whether restarting the runtime now would lose something only its process
 * knows. Core asks before maintenance stops the services, and counts no
 * answer as not ready.
 */
export type RestartReadiness =
  | { readonly state: "ready" }
  | { readonly state: "unready"; readonly reason: string };

/**
 * Keeps each turn core hands over and settles it later through core's
 * client. `submit` resolves once the turn is kept. Core hands a turn over
 * again after a restart, so the same correlation ID again is the same turn
 * and resolves again for any turn the runtime accepted before, including
 * one it finished whose outcome has not reached core yet. `control` acts on
 * a turn the runtime holds for that interface, as a person asked.
 */
export interface InboxTurns {
  submit(request: TurnRequest): Promise<void>;
  control(turnId: string, interfaceId: string, action: ControlAction): Promise<void>;
}

export interface RuntimeOperations {
  readonly turns: InboxTurns;
  /** The latest activity of a turn this instance is executing for that interface. */
  activity(correlationId: string, interfaceId: string): TurnActivity | undefined;
  modelState(query: ModelQuery): Promise<ModelState>;
  setModel(update: ModelUpdate): Promise<ModelState>;
  /** The engine's own commands a person may run in the agent's conversation, as the engine lists them now. */
  commands(query: ModelQuery): Promise<CommandCatalog>;
  restartReadiness(): RestartReadiness;
  close(): Promise<void>;
}
