/** Carillon's own state in an agent's pi-durable storage. */
import { createHash } from "node:crypto";
import type { JsonValue } from "@earendil-works/chord";
import {
  defineDoc,
  defineDocFamily,
  type ConversationId,
  type TaskId,
} from "@earendil-works/pi-durable";

/** Each Carillon conversation of the agent, by `conversationKey`: its pi-durable conversation. */
export const Sessions = defineDocFamily<{ conversation?: ConversationId }, null>({
  kind: "carillon.session",
  version: 1,
  scope: "session",
  family: true,
  initial: () => ({}),
  checkpointWhen: () => true,
});

/** The Carillon conversation a pi-durable conversation serves. */
export const ConversationInfo = defineDoc<{
  interface_id: string;
  agent_id: string;
  audience_id: string;
  /** The interface's metadata of the latest message admitted, such as its Discord origin. */
  native?: JsonValue;
  /** Where the conversation's attached files are kept, once any arrived. */
  attachment_root?: string;
}>({
  kind: "carillon.conversation",
  version: 1,
  scope: "conversation",
  history: "latest",
  fork: "current",
  initial: () => ({ interface_id: "", agent_id: "", audience_id: "" }),
  checkpointWhen: () => true,
});

/** Each turn core handed over, by turn ID: the task that settles it, once it has one. */
export const Turns = defineDocFamily<{ task?: TaskId }, null>({
  kind: "carillon.turn",
  version: 1,
  scope: "session",
  family: true,
  initial: () => ({}),
  checkpointWhen: () => true,
});

/** The agent's latest resource snapshot admitted with a turn. */
export const Resources = defineDoc<{ root?: string }>({
  kind: "carillon.resources",
  version: 1,
  scope: "session",
  initial: () => ({}),
  checkpointWhen: () => true,
});

/** The kind of the entry a turn's message is placed as. */
export const MESSAGE = "carillon.message";

/** A conversation of one agent: its interface and audience. */
export function conversationKey(interfaceId: string, audienceId: string): string {
  return createHash("sha256").update(interfaceId).update("\0").update(audienceId).digest("hex");
}
