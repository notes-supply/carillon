import type { Conversation } from "../api/stream.ts";

export function titleOf(conversation: Conversation): string {
  return conversation.title === "" ? "Files only" : conversation.title;
}
