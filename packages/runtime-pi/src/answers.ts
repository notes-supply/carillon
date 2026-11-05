/**
 * The files an agent attaches to its answer. `attach` checks that a path
 * names a file the runtime can read and records it with the conversation.
 * The conversation's next answer takes every file attached before it, in
 * the commit that decides the answer, so a restart before or after keeps
 * them; core then keeps a copy of each with the answer.
 */
import { defineTool, type ConversationId, type EntryId, type Tx } from "@earendil-works/pi-durable";
import { ANSWER_FILE_LIMIT } from "@carillon/protocol";
import { constants } from "node:fs";
import { open } from "node:fs/promises";
import { basename, resolve } from "node:path";
import { Type } from "typebox";
import { Attached } from "./documents.ts";

/**
 * Adds a file to the answer. A subagent's conversation is not offered it:
 * its answer goes to the agent that started it.
 */
export const ATTACH = defineTool({
  name: "attach",
  description:
    "Attaches a file to your answer, such as an image, a PDF or a CSV you made, so it reaches the person with your answer. Files go in the order you attach them, and attaching one again changes nothing. A relative path starts at your working directory.",
  parameters: Type.Object(
    { path: Type.String({ minLength: 1, description: "The file to attach." }) },
    { additionalProperties: false },
  ),
  // A file is recorded once by its path, so running again attaches nothing twice.
  replay: "safe",
  async execute({ path }, api, context) {
    const file = resolve(api.env?.cwd ?? "/", path);
    const size = await readable(file);
    const refusal = await api.commit(async (tx) => {
      const attached = await tx.doc(Attached, api.conversationId);
      if (attached.files.some((entry) => entry.path === file)) return undefined;
      if (attached.files.length >= ANSWER_FILE_LIMIT)
        return `An answer carries at most ${String(ANSWER_FILE_LIMIT)} files.`;
      const [newest] = (await tx.scanEntries({ conversationId: api.conversationId }, 1)).items;
      attached.files.push({ path: file, at: newest?.id ?? 0 });
      return undefined;
    }, context);
    if (refusal !== undefined) throw new Error(refusal);
    return {
      output: [
        { type: "text", text: `Attached ${basename(file)} (${size}); it goes with your answer.` },
      ],
    };
  },
});

/**
 * Takes the files attached in the conversation before its answer entry,
 * in the order attached; files attached after it wait for a later answer.
 */
export async function takeAttached(
  tx: Tx,
  conversation: ConversationId,
  answer: EntryId,
): Promise<string[]> {
  const attached = await tx.doc(Attached, conversation);
  const taken = attached.files.filter((file) => file.at < answer).map((file) => file.path);
  if (taken.length > 0) attached.files = attached.files.filter((file) => file.at >= answer);
  return taken;
}

/** The size of a regular file the runtime can read, or why it cannot attach it. */
async function readable(path: string): Promise<string> {
  const handle = await open(path, constants.O_RDONLY | constants.O_NONBLOCK).catch(
    (failure: unknown) => {
      throw new Error(unopened(path, failure));
    },
  );
  try {
    const stats = await handle.stat();
    if (!stats.isFile()) throw new Error(`${path} is not a regular file.`);
    return stats.size < 1024 * 1024
      ? `${String(Math.ceil(stats.size / 1024))} KiB`
      : `${(stats.size / (1024 * 1024)).toFixed(1)} MiB`;
  } finally {
    await handle.close();
  }
}

function unopened(path: string, failure: unknown): string {
  const code = failure instanceof Error && "code" in failure ? failure.code : undefined;
  if (code === "ENOENT") return `There is no file at ${path}.`;
  if (code === "EACCES") return `${path} cannot be read: permission denied.`;
  return `${path} cannot be opened: ${failure instanceof Error ? failure.message : String(failure)}`;
}
