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
  type Skill,
} from "@earendil-works/pi-coding-agent";
import {
  createRegistry,
  defineExtension,
  section,
  type AnyTask,
  type Extension,
  type UserInput,
} from "@earendil-works/pi-durable";
import type { Change, Setting, SettingValue, Subject } from "@carillon/protocol";
import { ExplainedFailure } from "@carillon/runtime/failure";
import { mismatch } from "@carillon/runtime/mismatch";
import { publish } from "@carillon/runtime/publish";
import { settingRevision } from "@carillon/runtime/settings";
import { readdir, readFile, stat } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { isDeepStrictEqual, stripVTControlCharacters } from "node:util";
import { Type, type Static } from "typebox";
import { Check } from "typebox/value";
import { ATTACH } from "./answers.ts";
import { attachmentSection } from "./attachments.ts";
import { ConversationInfo } from "./documents.ts";

/** What a message does when it arrives while the agent answers in its conversation. */
const BUSY = ["queue", "steer", "interrupt"] as const;
type Busy = (typeof BUSY)[number];
export const DEFAULT_BUSY: Busy = "steer";

export const BUILT_IN_SKILLS = new URL("../skills/", import.meta.url);

/** The runtime's own extension, always selected: the prompt from the agent's resources. */
const CARILLON = "carillon";
/** A module file, named without a comma, since `carillon settings` separates names with one. */
const USER_EXTENSION = /^extensions\/[^/,]+\.(?:js|mjs|ts)$/u;

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
    busy: Type.Optional(Type.Enum(BUSY)),
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
  /** For each module whose hooks are off while this snapshot is loaded, the handler that missed its deadline. */
  readonly slow: readonly SlowHook[];
  /** The skills a message may invoke; the prompt offers those not invocation-only. */
  readonly skills: readonly Skill[];
  /** Prompt sections, read once per snapshot so requests on it render the same prompt. */
  readonly prompt: {
    readonly system?: string;
    readonly context?: string;
    readonly skills?: string;
  };
}

/** A hook handler of a user extension: the extension's settings name, the task it hooks and its name. */
interface SlowHook {
  readonly extension: string;
  readonly task: string;
  readonly handler: string;
}

/** The most of a problem the agent reads and the log keeps: room for a dozen build errors. */
const PROBLEM_LIMIT = 4000;

/** Why a snapshot cannot be used, with its paths relative to its root, cut at `PROBLEM_LIMIT`. */
export interface Unloadable {
  readonly problem: string;
}

/** The extensions the runtime brings, and those settings select by default. */
export interface Extensions {
  /** The runtime's own extension, always selected. */
  readonly carillon: Extension;
  /** The built-in extensions, named as settings select them. */
  readonly builtIn: readonly Extension[];
  readonly defaults: readonly string[];
  /** How long a user extension may take to import. */
  readonly importDeadlineMs: number;
  /** How long a hook handler of a user extension may take. */
  readonly hookDeadlineMs: number;
}

/**
 * Core gives a runtime 10 seconds to take a turn before handing it over
 * again, and the oldest turn it cannot hand over holds back the runtime's
 * others. Half of that leaves admission room for its other work.
 */
export const IMPORT_DEADLINE_MS = 5_000;

/**
 * A handler holds its run, a Stop of that run and the runtime's close until
 * it returns, and systemd kills a service still stopping after 90 seconds.
 */
export const HOOK_DEADLINE_MS = 30_000;

/**
 * Reads a snapshot core captured: settings, prompt files and the listed
 * user extensions. Its skills join the runtime's `builtInSkills`, replacing
 * any of the same name. Settings or a module that cannot be used, also
 * beside the runtime's own extensions, make it unloadable.
 */
export async function loadSnapshot(
  root: string,
  workspace: string,
  extensions: Extensions,
  builtInSkills: string,
): Promise<Snapshot | Unloadable> {
  try {
    return await readSnapshot(root, workspace, extensions, builtInSkills);
  } catch (failure) {
    const text = failure instanceof Error ? failure.message : String(failure);
    const problem = stripVTControlCharacters(text)
      .replaceAll(`${pathToFileURL(root).href}/`, "")
      .replaceAll(`${root}/`, "");
    return {
      problem: problem.length > PROBLEM_LIMIT ? `${problem.slice(0, PROBLEM_LIMIT)}…` : problem,
    };
  }
}

async function readSnapshot(
  root: string,
  workspace: string,
  extensions: Extensions,
  builtInSkills: string,
): Promise<Snapshot> {
  const settings = await readSettings(join(root, "settings.json"));
  const selected = selection(settings.extensions, extensions.defaults);
  refuseUnknown(
    selected,
    extensions.builtIn.map((extension) => extension.name),
  );
  const slow: SlowHook[] = [];
  const modules = await Promise.all(
    selected.filter(isUserExtension).map((name) => importExtension(root, name, extensions, slow)),
  );
  const trial = createRegistry();
  for (const extension of [extensions.carillon, ...extensions.builtIn]) trial.install(extension);
  for (const module of modules)
    try {
      trial.install(module);
    } catch (failure) {
      throw new Error(
        `The agent's extension ${module.name} cannot be installed: ${failure instanceof Error ? failure.message : String(failure)}`,
        { cause: failure },
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
  const skills = loadSkills(root, builtInSkills);
  const skillsPrompt =
    reader === undefined || skills.length === 0 ? "" : formatSkillsForPrompt(skills, reader).trim();
  return {
    root,
    busy: settings.busy ?? DEFAULT_BUSY,
    selected,
    modules,
    slow,
    skills,
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
 * The skills of the resource root's `skills/` and the runtime's
 * `builtInSkills`, which one of the same name replaces, by name.
 */
export function loadSkills(root: string | undefined, builtInSkills: string): Skill[] {
  const own =
    root === undefined
      ? []
      : loadSkillsFromDir({ dir: join(root, "skills"), source: "carillon" }).skills;
  return [
    ...loadSkillsFromDir({ dir: builtInSkills, source: "builtin" }).skills.filter(
      (skill) => !own.some(({ name }) => name === skill.name),
    ),
    ...own,
  ].toSorted((a, b) => a.name.localeCompare(b.name));
}

/**
 * The runtime's own extension: the latest snapshot's prompt, the agent's
 * workspace and conversation store, where attached files are kept once a
 * conversation has any, `slowNote`, which names the hook handlers that are
 * off, and `attach`, which adds a file to the agent's answer.
 */
export function carillonExtension(
  latest: () => Snapshot | undefined,
  slowNote: () => string | undefined,
  workspace: string,
  store: string,
  tasks: readonly AnyTask[],
): Extension {
  return defineExtension({
    name: CARILLON,
    tools: [ATTACH],
    sections: [
      section("system", () => latest()?.prompt.system, { tag: false }),
      section("project_context", () => latest()?.prompt.context),
      section("skills", () => latest()?.prompt.skills, { tag: false }),
      section("cwd", () => workspace),
      section("conversation_store", () => store),
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
      section("hooks_off", slowNote, { tag: false }),
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

/** Where an agent's resources are edited, and what its settings may select. */
export interface ResourceSource {
  /** The agent's own resource root, which core captures with each turn; none without resources. */
  readonly root: string | undefined;
  /** The built-in extensions settings may select, by name. */
  readonly builtIn: readonly string[];
  readonly defaults: readonly string[];
}

/** The agent page's lines its resources' settings own. */
export const RESOURCE_SETTINGS: readonly string[] = ["busy", "extensions"];

/**
 * The agent's Busy and Extensions lines, as its resources set them now. An
 * agent without resources shows what it runs with and offers no choice.
 * Settings that cannot be used show both lines empty, so the rest of the
 * page stays usable; changing either names what is wrong. A selected
 * extension the resources no longer hold shows as unavailable, and a change
 * may drop it but not keep it.
 */
export async function resourceSettings(
  source: ResourceSource,
  subject: Subject,
): Promise<Setting[]> {
  try {
    return (await resourceLines(source, subject)).lines;
  } catch (failure) {
    if (!(failure instanceof ExplainedFailure)) throw failure;
    const unusable = (id: string, name: string): Setting => ({
      id,
      name,
      choices: [],
      value: null,
      inherited: false,
      revision: settingRevision(id, subject, failure.message),
    });
    return [unusable("busy", "Busy"), unusable("extensions", "Extensions")];
  }
}

/**
 * Applies `change` to the agent's own settings file while its line still
 * has the revision it expects, keeping everything else the file holds, its
 * mode included. The next turn's capture of the resources carries it.
 * `checkLease` throws once the caller's lease was lost.
 */
export async function changeResourceSetting(
  source: ResourceSource,
  subject: Subject,
  change: Change,
  checkLease: () => void,
): Promise<void> {
  const { lines, settings, offered } = await resourceLines(source, subject);
  const line = lines.find(({ id }) => id === change.setting);
  if (line === undefined || line.revision !== change.expected)
    throw new ExplainedFailure(409, "This setting changed meanwhile.");
  if (source.root === undefined)
    throw new ExplainedFailure(
      422,
      `${subject.agent_id} has no resources, so its ${line.name} cannot change.`,
    );
  const next: Settings = { ...settings };
  if (change.setting === "busy") {
    if (change.value === null) delete next.busy;
    else next.busy = busyMode(change.value);
  } else if (change.value === null) delete next.extensions;
  else next.extensions = extensionNames(change.value, offered);
  if (isDeepStrictEqual(next, settings)) return;
  const path = join(source.root, "settings.json");
  const mode = await stat(path).then(
    (file) => file.mode & 0o777,
    () => undefined,
  );
  checkLease();
  await publish(path, `${JSON.stringify(next, null, 2)}\n`, mode);
  checkLease();
}

async function resourceLines(source: ResourceSource, subject: Subject) {
  const root = source.root;
  const settings =
    root === undefined ? {} : await readable(() => readSettings(join(root, "settings.json")));
  const offered = [...source.builtIn, ...(await readable(() => userExtensions(root)))];
  const selected = selection(settings.extensions, source.defaults);
  const editable = source.root !== undefined;
  const busy: SettingValue = { kind: "one", id: settings.busy ?? DEFAULT_BUSY };
  const extensions: SettingValue = { kind: "many", ids: selected };
  const revision = (setting: string, stored: unknown, value: SettingValue) =>
    settingRevision(setting, subject, [editable, stored ?? null, value]);
  const lines: Setting[] = [
    {
      id: "busy",
      name: "Busy",
      choices: editable ? BUSY.map((mode) => ({ id: mode, name: mode })) : [],
      value: busy,
      inherited: settings.busy === undefined,
      revision: revision("busy", settings.busy, busy),
    },
    {
      id: "extensions",
      name: "Extensions",
      choices: editable
        ? [
            ...offered.map((name) => ({ id: name, name })),
            ...selected
              .filter((name) => !offered.includes(name))
              .map((name) => ({ id: name, name: `${name} (unavailable)` })),
          ]
        : [],
      value: extensions,
      inherited: settings.extensions === undefined,
      revision: revision("extensions", settings.extensions, extensions),
    },
  ];
  return { lines, settings, offered };
}

/** What `read` returns, or why the agent's resources could not be read, explained. */
async function readable<T>(read: () => Promise<T>): Promise<T> {
  try {
    return await read();
  } catch (failure) {
    if (failure instanceof ExplainedFailure) throw failure;
    throw new ExplainedFailure(
      422,
      `The agent's resources cannot be read: ${failure instanceof Error ? failure.message : String(failure)}`,
    );
  }
}

/** The user extensions the resources hold, as settings select them. */
async function userExtensions(root: string | undefined): Promise<string[]> {
  if (root === undefined) return [];
  const entries = await readdir(join(root, "extensions"), { withFileTypes: true }).catch(
    (failure: unknown) => {
      if (failure instanceof Error && "code" in failure && failure.code === "ENOENT") return [];
      throw failure;
    },
  );
  return entries
    .filter((entry) => entry.isFile())
    .map((entry) => `extensions/${entry.name}`)
    .filter(isUserExtension)
    .toSorted();
}

function busyMode(value: SettingValue): Busy {
  const mode = BUSY.find((candidate) => value.kind === "one" && candidate === value.id);
  if (mode === undefined)
    throw new ExplainedFailure(
      422,
      `Busy takes one of ${BUSY.join(", ")}${value.kind === "one" ? `, not ${value.id}` : ""}.`,
    );
  return mode;
}

function extensionNames(value: SettingValue, offered: readonly string[]): string[] {
  if (value.kind !== "many")
    throw new ExplainedFailure(422, "Extensions takes a list of extensions.");
  const unknown = value.ids.filter((name) => !offered.includes(name));
  if (unknown.length > 0)
    throw new ExplainedFailure(
      422,
      `${unknown.join(", ")} ${unknown.length === 1 ? "is not an extension" : "are not extensions"} here. The extensions are ${offered.join(", ")}.`,
    );
  return [...new Set(value.ids)];
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
  } catch (failure) {
    throw new ExplainedFailure(
      422,
      `The agent's settings.json is not valid JSON: ${failure instanceof Error ? failure.message : String(failure)}`,
    );
  }
  if (!Check(SettingsSchema, settings)) {
    const first = mismatch(SettingsSchema, settings);
    throw new ExplainedFailure(
      422,
      first?.keyword === "additionalProperties"
        ? `The agent's settings.json has settings this runtime does not know${first.instancePath.length === 0 ? "" : ` at ${first.instancePath}`}: ${first.params.additionalProperties.join(", ")}.`
        : `The agent's settings.json is invalid at ${first === undefined || first.instancePath.length === 0 ? "/" : first.instancePath}: ${first?.message ?? "unexpected value"}.`,
    );
  }
  return settings;
}

/** The extensions `setting` selects, in order and once each. */
function selection(setting: Settings["extensions"], defaults: readonly string[]): string[] {
  const names = Array.isArray(setting)
    ? setting
    : [
        ...defaults.filter((name) => setting?.remove?.includes(name) !== true),
        ...(setting?.add ?? []).filter((name) => setting?.remove?.includes(name) !== true),
      ];
  return [...new Set(names)];
}

/** Refuses a selection naming an extension the runtime has no way to load. */
function refuseUnknown(names: readonly string[], builtIn: readonly string[]): void {
  for (const name of names) {
    if (builtIn.includes(name) || isUserExtension(name)) continue;
    if (name.startsWith("mcp:"))
      throw new Error(
        `The agent's settings.json selects ${name}, but the Pi runtime's mcp.json configures no server ${name.slice(4)}.`,
      );
    throw new Error(
      `The agent's settings.json selects ${name}, which is neither a built-in extension (${builtIn.join(", ")}) nor a .js, .mjs or .ts file in extensions/ whose name has no comma.`,
    );
  }
}

const LATE = Symbol("late");

/** What `work` settles as, or `LATE` once `deadlineMs` passed. */
async function within(work: unknown, deadlineMs: number): Promise<unknown> {
  let timer: NodeJS.Timeout | undefined;
  const late = new Promise((resolve) => {
    timer = setTimeout(resolve, deadlineMs, LATE);
  });
  try {
    return await Promise.race([work, late]);
  } finally {
    clearTimeout(timer);
  }
}

/**
 * A user extension: its module's default export, a pi-durable extension
 * object. Its top-level code runs in the runtime's process, and a module
 * still importing after its deadline cannot be used. Its hook handlers get
 * a deadline too.
 */
async function importExtension(
  root: string,
  name: string,
  { importDeadlineMs, hookDeadlineMs }: Extensions,
  slow: SlowHook[],
): Promise<Extension> {
  let module: unknown;
  try {
    module = await within(import(pathToFileURL(join(root, name)).href), importDeadlineMs);
    if (module === LATE)
      throw new Error(
        `its module did not finish importing within ${String(importDeadlineMs / 1000)} seconds`,
      );
  } catch (failure) {
    throw new Error(
      `The agent's extension ${name} could not be loaded: ${importFailure(failure, root)}`,
      { cause: failure },
    );
  }
  const extension =
    typeof module === "object" && module !== null && "default" in module
      ? module.default
      : undefined;
  if (!isExtensionBody(extension)) {
    const first = mismatch(ExtensionBodySchema, extension);
    throw new Error(
      `The agent's extension ${name} must export a pi-durable extension object as its default export${first === undefined || first.instancePath.length === 0 ? "" : `: ${first.instancePath} ${first.message}`}.`,
    );
  }
  if (extension.hooks === undefined) return { ...extension, name };
  const off = () => slow.some((hook) => hook.extension === name);
  // pi-durable reads a hook as `handlers[name]`, so inherited methods count,
  // and takes `undefined` from any hook as no change. A late handler must not
  // throw: a throw from `beforeTool` blocks the tools the agent repairs its
  // files with.
  const hooks = extension.hooks.map(({ task, handlers }) => {
    const get = (_target: object, handler: string | symbol): unknown => {
      const run: unknown = Reflect.get(handlers, handler);
      if (typeof run !== "function" || typeof handler !== "string") return run;
      return async (...args: unknown[]): Promise<unknown> => {
        const result = off()
          ? LATE
          : await within(Reflect.apply(run, handlers, args), hookDeadlineMs);
        if (result !== LATE) return result;
        if (!off()) {
          slow.push({ extension: name, task, handler });
          console.error(
            `extension ${name}: its ${task} hook ${handler} did not finish within ${String(hookDeadlineMs / 1000)} seconds, so its hooks are off`,
          );
        }
        return undefined;
      };
    };
    return { task, handlers: new Proxy({}, { get }) };
  });
  return { ...extension, name, hooks };
}

/** Where Bun's build errors point, which only their `position` holds. */
const PositionSchema = Type.Object({
  file: Type.String(),
  line: Type.Number(),
  column: Type.Number(),
});

/**
 * Why a module did not import: each of Bun's build errors at its place, or
 * the error with the stack frames in the snapshot's files.
 */
function importFailure(failure: unknown, root: string): string {
  if (failure instanceof AggregateError)
    return [failure.message, ...failure.errors.map((error) => importFailure(error, root))].join(
      "\n",
    );
  if (!(failure instanceof Error)) return String(failure);
  const position = "position" in failure ? failure.position : undefined;
  if (Check(PositionSchema, position))
    return `${position.file}:${String(position.line)}:${String(position.column)}: ${failure.message}`;
  const frames = (failure.stack ?? "")
    .split("\n")
    .filter((line) => /^\s+at /u.test(line) && line.includes(root))
    .map((line) => line.trim());
  return [failure.message, ...frames].join("\n");
}

/** The input with a `[Carillon]` note about the agent's resources before it. */
export function noted(content: UserInput, note: string | undefined): UserInput {
  if (note === undefined) return content;
  if (typeof content === "string") return `${note}\n\n${content}`;
  return [{ type: "text", text: note }, ...content];
}

/** Whether settings name a user extension, a module of the snapshot, rather than a built-in. */
export function isUserExtension(name: string): boolean {
  return USER_EXTENSION.test(name);
}

/** An extension's parts; its name is the one settings select it by. */
function isExtensionBody(value: unknown): value is Omit<Extension, "name"> {
  return Check(ExtensionBodySchema, value);
}
