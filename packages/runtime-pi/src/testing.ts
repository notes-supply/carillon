/** Service-test substitute. Does not exercise Pi or make model calls. */
import type { AvailableModel, Selection } from "@carillon/protocol";
import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile, rename } from "node:fs/promises";
import { join } from "node:path";
import { Type } from "typebox";
import { Parse } from "typebox/value";
import { capturedPath } from "./attachments.ts";
import { ModelSelections } from "./model.ts";
import { Serial } from "@carillon/runtime/serial";
import { conversationDirectory } from "./namespace.ts";
import type { RuntimeOperations, TurnRequest, TurnResponse } from "@carillon/runtime/operations";
interface DeterministicSession {
  readonly turns: readonly {
    readonly correlation_id: string;
    readonly input: string;
    readonly output: string;
  }[];
}

const DeterministicSessionSchema = Type.Object({
  turns: Type.Array(
    Type.Object({
      correlation_id: Type.String(),
      input: Type.String(),
      output: Type.String(),
    }),
  ),
});

export async function createDeterministicRuntime(
  stateDirectory: string,
  sessionDirectory: string,
): Promise<RuntimeOperations> {
  await mkdir(stateDirectory, { recursive: true, mode: 0o700 });
  const selections = await ModelSelections.open(join(stateDirectory, "model-selection.json"));
  const turns = new Serial();
  const shutdown = new AbortController();
  return {
    executeTurn(request, signal) {
      return turns.run(() => {
        shutdown.signal.throwIfAborted();
        signal?.throwIfAborted();
        return executeDeterministic(request);
      });
    },
    activity() {
      return undefined;
    },
    async modelState(query) {
      shutdown.signal.throwIfAborted();
      return await selections.state(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, query);
    },
    setModel(update) {
      shutdown.signal.throwIfAborted();
      return selections.update(DETERMINISTIC_MODELS, DETERMINISTIC_DEFAULT, update);
    },
    commands() {
      shutdown.signal.throwIfAborted();
      return Promise.resolve({ commands: [] });
    },
    async close() {
      shutdown.abort();
      await turns.run(() => Promise.resolve());
    },
  };
  async function executeDeterministic(request: TurnRequest): Promise<TurnResponse> {
    // Resolved first, as in `executePi`: a change made while the turn runs
    // applies to the next one.
    const selection = await selections.effective(
      DETERMINISTIC_MODELS,
      DETERMINISTIC_DEFAULT,
      request,
    );
    const directory = conversationDirectory(sessionDirectory, request);
    await mkdir(directory, { recursive: true });
    const sessionPath = join(directory, "deterministic.json");
    const prior = await readDeterministicSession(sessionPath);
    const extensionActive = await exists(
      join(request.resource_root, "extensions", "carillon-context.js"),
    );
    const skillActive = await exists(
      join(request.resource_root, "skills", "carillon-status", "SKILL.md"),
    );
    const markers = [
      extensionActive ? "native extension active" : "native extension inactive",
      skillActive ? "skill active" : "skill inactive",
      `model ${describeSelection(selection)}`,
      ...(await describeAttachments(request)),
    ];
    const output = `Local provider response ${String(prior.turns.length + 1)}: ${request.input}. ${markers.join("; ")}.`;
    const next: DeterministicSession = {
      turns: [
        ...prior.turns,
        { correlation_id: request.correlation_id, input: request.input, output },
      ],
    };
    await atomicJson(sessionPath, next);
    return {
      correlation_id: request.correlation_id,
      output,
    };
  }
}
/** Each attached file as found under the attachment root, checked against its digest. */
async function describeAttachments(request: TurnRequest): Promise<string[]> {
  return await Promise.all(
    (request.attachments ?? []).map(async (attachment) => {
      const bytes = await readFile(capturedPath(request.attachment_root, attachment));
      const intact = createHash("sha256").update(bytes).digest("hex") === attachment.sha256;
      return `attachment ${attachment.name} ${String(bytes.length)} bytes ${intact ? "intact" : "altered"}`;
    }),
  );
}

async function readDeterministicSession(path: string): Promise<DeterministicSession> {
  try {
    const content = await readFile(path, "utf8");
    return await Parse(DeterministicSessionSchema, JSON.parse(content));
  } catch (failure: unknown) {
    if (isNodeError(failure) && failure.code === "ENOENT") return { turns: [] };
    throw failure;
  }
}

async function atomicJson(path: string, value: unknown): Promise<void> {
  const temporary = `${path}.${String(process.pid)}.tmp`;
  await writeFile(temporary, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
  await rename(temporary, path);
}

async function exists(path: string): Promise<boolean> {
  try {
    await readFile(path);
    return true;
  } catch (failure: unknown) {
    if (isNodeError(failure) && failure.code === "ENOENT") return false;
    throw failure;
  }
}

function isNodeError(value: unknown): value is NodeJS.ErrnoException {
  return value instanceof Error && "code" in value;
}

/** The catalog the deterministic provider substitute offers. */
export const DETERMINISTIC_MODELS: readonly AvailableModel[] = [
  { provider: "local", model: "echo", name: "Local echo", options: [] },
  {
    provider: "local",
    model: "reasoner",
    name: "Local reasoner",
    options: [
      {
        id: "thinking",
        name: "Thinking",
        choices: ["off", "low", "medium", "high"],
        default: "medium",
      },
    ],
  },
  { provider: "substitute", model: "fixed", name: "Substitute fixed", options: [] },
];

export const DETERMINISTIC_DEFAULT: Selection = { provider: "local", model: "echo", options: {} };

/** `provider/model` with any chosen options, for output that names the model. */
export function describeSelection(selection: Selection): string {
  const options = Object.entries(selection.options ?? {})
    .map(([id, choice]) => `${id}=${choice}`)
    .join(", ");
  return `${selection.provider}/${selection.model}${options.length === 0 ? "" : ` (${options})`}`;
}
