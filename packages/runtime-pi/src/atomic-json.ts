import { randomUUID } from "node:crypto";
import { rename, rm, writeFile } from "node:fs/promises";

export async function atomicJson(path: string, value: unknown): Promise<void> {
  const temporary = `${path}.${String(process.pid)}.${randomUUID()}.tmp`;
  try {
    await writeFile(temporary, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
    await rename(temporary, path);
  } catch (failure: unknown) {
    await rm(temporary, { force: true }).catch(() => undefined);
    throw failure;
  }
}
