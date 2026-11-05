/** Durable replacement of a file a runtime owns. */
import { open, rename, rm } from "node:fs/promises";
import { dirname } from "node:path";
import { ExplainedFailure } from "./failure.ts";

/**
 * A publication in place whose directory did not sync, so a crash may still
 * undo it. It tells the caller the change was made.
 */
export class Unsynced extends ExplainedFailure {
  constructor(message: string) {
    super(500, message);
  }
}

/**
 * Replaces `path` with `text` and `mode`, readable only by the owner unless
 * the mode says otherwise, durably. A failure before the rename leaves `path`
 * as it was; {@link Unsynced} leaves the new text and mode in place.
 */
export async function publish(path: string, text: string, mode = 0o600): Promise<void> {
  const temporary = `${path}.${crypto.randomUUID()}.tmp`;
  const file = await open(temporary, "wx", 0o600);
  try {
    try {
      await file.chmod(mode);
      await file.writeFile(text);
      await file.sync();
    } finally {
      await file.close();
    }
    await rename(temporary, path);
  } catch (failure) {
    await rm(temporary, { force: true });
    throw failure;
  }
  try {
    const directory = await open(dirname(path), "r");
    try {
      await directory.sync();
    } finally {
      await directory.close();
    }
  } catch (failure) {
    const reason = failure instanceof Error ? failure.message : String(failure);
    throw new Unsynced(`${path}: changed, but not confirmed durable: ${reason}`);
  }
}
