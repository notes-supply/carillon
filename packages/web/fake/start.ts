import { spawn } from "node:child_process";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { Type } from "typebox";
import { readAnswer } from "../src/api/requests.ts";

const ListeningSchema = Type.Object({ port: Type.Integer() });

/** What `/fake/state` and `/fake/step` answer, as far as a script reads them. */
export const StateSchema = Type.Object({
  written: Type.Integer(),
  ended: Type.Integer(),
  remaining: Type.Array(Type.String()),
});
export const SteppedSchema = Type.Object({ label: Type.String(), wrote_at: Type.Number() });

export type Started = { origin: string; stop(): void };

/** Runs the fake as a process of its own, on a free port and the manual clock. */
export async function start(scenario: string, site?: string): Promise<Started> {
  const fake = spawn(
    process.execPath,
    [
      join(import.meta.dirname, "main.ts"),
      "--port=0",
      "--clock=manual",
      `--scenario=${scenario}`,
      ...(site === undefined ? [] : [`--site=${site}`]),
    ],
    { stdio: ["ignore", "pipe", "inherit"] },
  );
  for await (const line of createInterface(fake.stdout)) {
    const listening = readAnswer(ListeningSchema, line);
    if (listening !== undefined)
      return { origin: `http://127.0.0.1:${String(listening.port)}`, stop: () => void fake.kill() };
  }
  throw new Error("the fake ended before it said which port it listens on");
}
