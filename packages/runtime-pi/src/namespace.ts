import { SessionManager } from "@earendil-works/pi-coding-agent";
import { createHash } from "node:crypto";
import { join } from "node:path";

interface Conversation {
  readonly interface_id: string;
  readonly agent_id: string;
  readonly audience_id: string;
}

export function conversationDirectory(root: string, conversation: Conversation): string {
  const hash = createHash("sha256");
  for (const value of [conversation.interface_id, conversation.agent_id, conversation.audience_id])
    hash.update(value).update("\0");
  return join(root, hash.digest("hex"));
}

/** Pi owns session selection and persistence, even when no turn completed. */
export function openConversation(workspace: string, root: string, conversation: Conversation) {
  return SessionManager.continueRecent(workspace, conversationDirectory(root, conversation));
}
