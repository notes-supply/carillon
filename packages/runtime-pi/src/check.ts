/**
 * `carillon runtime-pi check-resources [--agent ID]`: loads an agent's own
 * folder as the runtime loads the resources a message brings, so the agent
 * can check a change before it relies on it. The load runs in a child
 * process, so a module's top-level code cannot touch the running runtime,
 * and a module that ends the process or never finishes fails the check
 * instead of passing it or hanging the caller.
 */
import { spawn } from "node:child_process";
import { once } from "node:events";
import { Readable } from "node:stream";
import { Type, type Static } from "typebox";
import { Check } from "typebox/value";

/** The child's mode, which only `check-resources` starts. */
export const LOAD_RESOURCES = "load-resources";

/** Where the child reports, apart from the output a module may write. */
export const REPORT_FD = 3;

/** How long the child may take, startup included, before it is stopped. */
const CHECK_DEADLINE_MS = 10_000;

const ReportSchema = Type.Union([
  Type.Object({ kind: Type.Literal("loading"), folder: Type.String() }),
  Type.Object({ kind: Type.Literal("done"), ok: Type.Boolean(), text: Type.String() }),
]);

/** A line the child reports: the folder it starts loading, then whether it loads. */
export type Report = Static<typeof ReportSchema>;

/**
 * Runs this executable, with `self` before its arguments, in the child's
 * mode with `args`, prints what it found and returns the exit code: 0 when
 * the folder loads.
 */
export async function checkResources(
  self: readonly string[],
  args: readonly string[],
): Promise<number> {
  const child = spawn(process.execPath, [...self, LOAD_RESOURCES, ...args], {
    stdio: ["ignore", "inherit", "inherit", "pipe"],
  });
  const channel = child.stdio[REPORT_FD];
  if (!(channel instanceof Readable)) throw new Error("the check's report channel did not open");
  const reports = reportsOf(channel);
  let stopped = false;
  const timer = setTimeout(() => {
    stopped = true;
    child.kill("SIGKILL");
  }, CHECK_DEADLINE_MS);
  await once(child, "close");
  clearTimeout(timer);
  const lines = await reports;
  const done = lines.find((line) => line.kind === "done");
  if (done !== undefined) {
    (done.ok ? process.stdout : process.stderr).write(`${done.text}\n`);
    return done.ok ? 0 : 1;
  }
  const loading = lines.find((line) => line.kind === "loading");
  const folder = loading === undefined ? "The agent's resources" : loading.folder;
  process.stderr.write(
    stopped
      ? `${folder} did not finish loading within ${String(CHECK_DEADLINE_MS / 1000)} seconds: a module's top-level code may never end. In the runtime, that stops every agent.\n`
      : `${folder} did not finish loading: something ended the process (${child.signalCode ?? `exit code ${String(child.exitCode)}`}), such as a module calling process.exit(). In the runtime, that stops every agent.\n`,
  );
  return 1;
}

/** Lines that are not reports, such as a module's writes to the same descriptor, are skipped. */
async function reportsOf(stream: Readable): Promise<Report[]> {
  let text = "";
  for await (const chunk of stream) text += String(chunk);
  return text.split("\n").flatMap((line) => {
    if (line.length === 0) return [];
    let value: unknown;
    try {
      value = JSON.parse(line);
    } catch {
      return [];
    }
    return Check(ReportSchema, value) ? [value] : [];
  });
}
