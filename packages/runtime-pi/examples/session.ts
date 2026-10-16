/** Direct native Pi execution: no Carillon HTTP server or Discord. */
import { parseArgs } from "node:util";
import { join, resolve } from "node:path";
import type { RuntimeOperations } from "@carillon/runtime/operations";
import { createPiRuntime } from "@carillon/runtime-pi";

/** The same example is exercised with Pi's real adapter and a controlled model in tests. */
export async function runSessionExample(runtime: RuntimeOperations, resourceRoot: string) {
  const conversation = {
    interface_id: "example",
    agent_id: "example",
    audience_id: "native-session",
    resource_root: resourceRoot,
    attachment_root: join(resourceRoot, "attachments"),
  };
  const first = await runtime.executeTurn({
    ...conversation,
    correlation_id: "example-first",
    input: "Remember the word carillon. Reply briefly.",
  });
  const second = await runtime.executeTurn({
    ...conversation,
    correlation_id: "example-second",
    input: "Which word did I ask you to remember?",
  });
  return [first.output, second.output];
}

if (import.meta.main) {
  const { values } = parseArgs({
    options: {
      "state-dir": { type: "string" },
      workspace: { type: "string", default: process.cwd() },
      resources: { type: "string", default: process.cwd() },
    },
  });
  if (values["state-dir"] === undefined)
    throw new Error(
      "Pass --state-dir with configured Pi auth.json and settings.json; see the package README.",
    );
  const runtime = await createPiRuntime({
    agents: [{ id: "example", workspace: resolve(values.workspace) }],
    stateDirectory: resolve(values["state-dir"]),
  });
  try {
    for (const output of await runSessionExample(runtime, resolve(values.resources)))
      console.log(output);
  } finally {
    await runtime.close();
  }
}
