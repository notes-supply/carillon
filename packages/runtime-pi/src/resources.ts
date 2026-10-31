/**
 * An agent's Carillon resources as the durable runtime uses them: the
 * snapshot core captured with a turn, with its settings, prompt files,
 * skills and the user extensions it lists. The latest snapshot admitted
 * wins: its prompt and extensions apply to the agent's next requests,
 * including those of messages admitted earlier.
 */
import {
  formatSkillsForPrompt,
  loadProjectContextFiles,
  loadSkillsFromDir,
} from "@earendil-works/pi-coding-agent";
import {
  createRegistry,
  defineExtension,
  section,
  type AnyTask,
  type Extension,
} from "@earendil-works/pi-durable";
import { ExplainedFailure } from "@carillon/runtime/failure";
import { mismatch } from "@carillon/runtime/mismatch";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { Type, type Static } from "typebox";
import { Check } from "typebox/value";
import { attachmentSection } from "./attachments.ts";
import { ConversationInfo } from "./documents.ts";

/** What a message does when it arrives while the agent answers in its conversation. */
type Busy = "queue" | "steer" | "interrupt";

/** The runtime's own extension, always selected: the prompt from the agent's resources. */
const CARILLON = "carillon";
const USER_EXTENSION = /^extensions\/[^/]+\.(?:js|mjs|ts)$/u;

const NameSchema = Type.String({ minLength: 1 });
const SettingsSchema = Type.Object(
  {
    extensions: Type.Optional(
      Type.Union([
        Type.Array(NameSchema),
        Type.Object(
          {
            add: Type.Optional(Type.Array(NameSchema)),
            remove: Type.Optional(Type.Array(NameSchema)),
          },
          { additionalProperties: false },
        ),
      ]),
    ),
    busy: Type.Optional(
      Type.Union([Type.Literal("queue"), Type.Literal("steer"), Type.Literal("interrupt")]),
    ),
  },
  { additionalProperties: false },
);
type Settings = Static<typeof SettingsSchema>;

const FunctionSchema = Type.Function([], Type.Unknown());
/**
 * The parts of a user extension's default export pi-durable uses as it
 * prepares requests, and its tasks' names. pi-durable's registry checks
 * only names and section keys as it installs it.
 */
const ExtensionBodySchema = Type.Object({
  tools: Type.Optional(
    Type.Array(
      Type.Object({
        name: Type.String(),
        description: Type.String(),
        parameters: Type.Object({}),
        execute: FunctionSchema,
      }),
    ),
  ),
  sections: Type.Optional(Type.Array(Type.Object({ key: Type.String(), render: FunctionSchema }))),
  hooks: Type.Optional(Type.Array(Type.Object({ task: Type.String(), handlers: Type.Object({}) }))),
  wraps: Type.Optional(
    Type.Array(
      Type.Union([
        Type.Object({ tool: Type.String(), wrap: FunctionSchema }),
        Type.Object({ section: Type.String(), wrap: FunctionSchema }),
      ]),
    ),
  ),
  tasks: Type.Optional(
    Type.Array(Type.Object({ definition: Type.Object({ name: Type.String() }) })),
  ),
});

export interface Snapshot {
  readonly root: string;
  readonly busy: Busy;
  /** The selected extensions by name, in order. */
  readonly selected: readonly string[];
  /** The user extensions the snapshot lists, each named as its settings select it. */
  readonly modules: readonly Extension[];
  /** Prompt sections, read once per snapshot so requests on it render the same prompt. */
  readonly prompt: {
    readonly system?: string;
    readonly context?: string;
    readonly skills?: string;
  };
}

/** The extensions the runtime brings, and those settings select by default. */
export interface Extensions {
  /** The runtime's own extension, always selected. */
  readonly carillon: Extension;
  /** The built-in extensions, named as settings select them. */
  readonly builtIn: readonly Extension[];
  readonly defaults: readonly string[];
}

/**
 * Reads a snapshot core captured: settings, prompt files and the listed
 * user extensions. Settings or a module that cannot be used, also beside
 * the runtime's own extensions, refuse the turn that brought them.
 */
export async function loadSnapshot(
  root: string,
  workspace: string,
  extensions: Extensions,
): Promise<Snapshot> {
  const settings = await readSettings(join(root, "settings.json"));
  const selected = selection(settings.extensions, extensions);
  const modules = await Promise.all(
    selected.filter(isUserExtension).map((name) => importExtension(root, name)),
  );
  const trial = createRegistry();
  for (const extension of [extensions.carillon, ...extensions.builtIn]) trial.install(extension);
  for (const module of modules)
    try {
      trial.install(module);
    } catch (failure) {
      throw new ExplainedFailure(
        422,
        `The agent's extension ${module.name} cannot be installed: ${failure instanceof Error ? failure.message : String(failure)}`,
      );
    }
  const system = await readFile(join(root, "SYSTEM.md"), "utf8").catch((failure: unknown) => {
    if (failure instanceof Error && "code" in failure && failure.code === "ENOENT")
      return undefined;
    throw failure;
  });
  const context = loadProjectContextFiles({ cwd: workspace, agentDir: root });
  const reader = selected.includes("coding")
    ? "read"
    : selected.includes("codemode")
      ? "indirect"
      : undefined;
  const { skills } = loadSkillsFromDir({ dir: join(root, "skills"), source: "carillon" });
  const skillsPrompt =
    reader === undefined || skills.length === 0 ? "" : formatSkillsForPrompt(skills, reader).trim();
  return {
    root,
    busy: settings.busy ?? "steer",
    selected,
    modules,
    prompt: {
      ...(system === undefined || system.trim().length === 0 ? {} : { system }),
      ...(context.length === 0
        ? {}
        : {
            context: [
              "Project-specific instructions and guidelines:",
              ...context.map(
                ({ path, content }) =>
                  `<project_instructions path="${path}">\n${content}\n</project_instructions>`,
              ),
            ].join("\n\n"),
          }),
      ...(skillsPrompt.length === 0 ? {} : { skills: skillsPrompt }),
    },
  };
}

/**
 * The runtime's own extension: the latest snapshot's prompt, the agent's
 * workspace, and where attached files are kept once a conversation has any.
 */
export function carillonExtension(
  latest: () => Snapshot | undefined,
  workspace: string,
  tasks: readonly AnyTask[],
): Extension {
  return defineExtension({
    name: CARILLON,
    sections: [
      section("system", () => latest()?.prompt.system, { tag: false }),
      section("project_context", () => latest()?.prompt.context),
      section("skills", () => latest()?.prompt.skills, { tag: false }),
      section("cwd", () => workspace),
      section("attachments", async (input, context) => {
        const conversation = await input.read.snapshot(
          ConversationInfo,
          input.conversationId,
          context,
        );
        return conversation?.attachment_root === undefined
          ? undefined
          : attachmentSection(conversation.attachment_root);
      }),
    ],
    tasks,
  });
}

/** The runtime's extension first, then the snapshot's selection as the registry holds it. */
export function selectedExtensions(
  snapshot: Snapshot | undefined,
  defaults: readonly string[],
  installed: (name: string) => Extension | undefined,
): Extension[] {
  return [CARILLON, ...(snapshot?.selected ?? defaults)].flatMap((name) => {
    const extension = installed(name);
    return extension === undefined ? [] : [extension];
  });
}

async function readSettings(path: string): Promise<Settings> {
  let text: string;
  try {
    text = await readFile(path, "utf8");
  } catch (failure) {
    if (failure instanceof Error && "code" in failure && failure.code === "ENOENT") return {};
    throw failure;
  }
  let settings: unknown;
  try {
    settings = JSON.parse(text);
  } catch {
    throw new ExplainedFailure(422, "The agent's settings.json is not valid JSON.");
  }
  if (!Check(SettingsSchema, settings)) {
    const first = mismatch(SettingsSchema, settings);
    throw new ExplainedFailure(
      422,
      first?.keyword === "additionalProperties"
        ? `The agent's settings.json has settings this runtime does not know: ${first.params.additionalProperties.join(", ")}.`
        : `The agent's settings.json is invalid at ${first === undefined || first.instancePath.length === 0 ? "/" : first.instancePath}: ${first?.message ?? "unexpected value"}.`,
    );
  }
  return settings;
}

function selection(setting: Settings["extensions"], extensions: Extensions): string[] {
  const names = Array.isArray(setting)
    ? setting
    : [
        ...extensions.defaults.filter((name) => setting?.remove?.includes(name) !== true),
        ...(setting?.add ?? []).filter((name) => setting?.remove?.includes(name) !== true),
      ];
  const builtIn = extensions.builtIn.map((extension) => extension.name);
  for (const name of names) {
    if (builtIn.includes(name) || isUserExtension(name)) continue;
    if (name.startsWith("mcp:"))
      throw new ExplainedFailure(
        422,
        `The agent's settings.json selects ${name}, but the Pi runtime's mcp.json configures no server ${name.slice(4)}.`,
      );
    throw new ExplainedFailure(
      422,
      `The agent's settings.json selects ${name}, which is neither a built-in extension (${builtIn.join(", ")}) nor extensions/<file>.js, .mjs or .ts.`,
    );
  }
  return [...new Set(names)];
}

/** A user extension: its module's default export, a pi-durable extension object. */
async function importExtension(root: string, name: string): Promise<Extension> {
  let module: unknown;
  try {
    module = await import(pathToFileURL(join(root, name)).href);
  } catch (failure) {
    throw new ExplainedFailure(
      422,
      `The agent's extension ${name} could not be loaded: ${failure instanceof Error ? failure.message : String(failure)}`,
    );
  }
  const extension =
    typeof module === "object" && module !== null && "default" in module
      ? module.default
      : undefined;
  if (!isExtensionBody(extension)) {
    const first = mismatch(ExtensionBodySchema, extension);
    throw new ExplainedFailure(
      422,
      `The agent's extension ${name} must export a pi-durable extension object as its default export${first === undefined || first.instancePath.length === 0 ? "" : `: ${first.instancePath} ${first.message}`}.`,
    );
  }
  return { ...extension, name };
}

/** Whether settings name a user extension, a module of the snapshot, rather than a built-in. */
export function isUserExtension(name: string): boolean {
  return USER_EXTENSION.test(name);
}

/** An extension's parts; its name is the one settings select it by. */
function isExtensionBody(value: unknown): value is Omit<Extension, "name"> {
  return Check(ExtensionBodySchema, value);
}
