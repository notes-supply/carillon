/**
 * The files an agent attaches to its answer. `attach` checks that a path
 * names a file the runtime can read and records it with the run that
 * attached it. That run's answer takes them, in the commit that decides
 * it, so a restart before or after keeps them; core then keeps a copy of
 * each with the answer. A run that ends without an answer drops them.
 */
import type { Context } from "@earendil-works/chord";
import {
  defineTool,
  LiveDoc,
  type ConversationId,
  type DocumentReader,
  type EntryId,
  type SubmissionId,
  type SubmissionRecord,
  type Tx,
} from "@earendil-works/pi-durable";
import { ANSWER_FILE_LIMIT } from "@carillon/protocol";
import { constants } from "node:fs";
import { open } from "node:fs/promises";
import { basename, resolve } from "node:path";
import { Type } from "typebox";
import { size } from "./attachments.ts";
import { Attached } from "./documents.ts";

export const ATTACH = defineTool({
  name: "attach",
  description:
    "Attaches a file to your answer, such as an image, a PDF or a CSV you made, so it reaches the person with your answer. Files go in the order you attach them, and attaching one again changes nothing. A relative path starts at your working directory. If you are stopped or interrupted before you answer, or your answer fails, its files are dropped, so attach them again with your next answer.",
  parameters: Type.Object(
    { path: Type.String({ minLength: 1, description: "The file to attach." }) },
    { additionalProperties: false },
  ),
  replay: "safe",
  // Calls in one round commit in the order they finish unless they run in
  // turn, and files go in the order they were attached.
  executionMode: "sequential",
  async execute({ path }, api, context) {
    const file = resolve(api.env?.cwd ?? "/", path);
    const size = await readable(file);
    // A tool call runs within its run, whose first input stays its first.
    const run = (await api.snapshot(LiveDoc, api.conversationId, context))?.run?.inputs[0];
    if (run === undefined) throw new Error("A file is attached only while you answer a message.");
    const refusal = await api.commit(async (tx) => {
      const attached = await tx.doc(Attached, api.conversationId);
      const ofRun = attached.files.filter((entry) => entry.run === run);
      if (ofRun.some((entry) => entry.path === file)) return undefined;
      if (ofRun.length >= ANSWER_FILE_LIMIT)
        return `An answer carries at most ${String(ANSWER_FILE_LIMIT)} files.`;
      attached.files.push({ path: file, run });
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
 * How each run that attached files ended, by its first input: with its
 * answer entry, or `null` without one. A run still going is absent.
 */
export type RunEnds = ReadonlyMap<SubmissionId, EntryId | null>;

/** How the runs that attached files in the conversation have ended so far. */
export async function runEnds(
  reader: DocumentReader,
  submissions: {
    status(submission: SubmissionId, context: Context): Promise<SubmissionRecord | undefined>;
  },
  conversation: ConversationId,
  context: Context,
): Promise<RunEnds> {
  const attached = await reader.snapshot(Attached, conversation, context);
  const ends = new Map<SubmissionId, EntryId | null>();
  for (const run of new Set(attached?.files.map((file) => file.run))) {
    const record = await submissions.status(run, context);
    // Every input of a run settles with it, its first included.
    if (record === undefined || record.status === "unanswered") ends.set(run, null);
    else if (record.status === "done") ends.set(run, record.answer ?? null);
  }
  return ends;
}

/**
 * Takes the files of the run that ended with `answer`, in the order
 * attached, and drops those of runs `ends` says ended without one. Files of
 * a run still going, or of another answer, stay for it.
 */
export async function takeAttached(
  tx: Tx,
  conversation: ConversationId,
  ends: RunEnds,
  answer: EntryId | null,
): Promise<string[]> {
  const attached = await tx.doc(Attached, conversation);
  const taken = attached.files.filter((file) => answer !== null && ends.get(file.run) === answer);
  const staying = attached.files.filter((file) => {
    const end = ends.get(file.run);
    return end === undefined || (end !== null && end !== answer);
  });
  if (staying.length < attached.files.length) attached.files = staying;
  return taken.map((file) => file.path);
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
    return size(stats.size);
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
