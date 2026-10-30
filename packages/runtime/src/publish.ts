/** Durable replacement of a file a runtime owns. */
import { open, rename, rm } from "node:fs/promises";
import { dirname } from "node:path";

/** A publication in place whose directory did not sync, so a crash may still undo it. */
export class Unsynced extends Error {}

/**
 * Replaces `path` with `text`, readable only by the owner, durably. A failure
 * before the rename leaves `path` as it was; {@link Unsynced} leaves the new
 * text in place.
 */
export async function publish(path: string, text: string): Promise<void> {
  const temporary = `${path}.${crypto.randomUUID()}.tmp`;
  const file = await open(temporary, "wx", 0o600);
  try {
    try {
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
