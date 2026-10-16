/**
 * Deny-by-default descriptions of what a turn's tools act on, bounded for
 * display and shared by every runtime: a known tool shows the field that
 * says what it acts on, a tool shaped like an operation shows its action and
 * numeric target IDs, and anything else shows only its name. Every detail
 * passes the same best-effort credential redaction as diagnostics before it
 * is cut to length, so no caller ever receives raw tool arguments.
 */
import {
  ACTIVITY_REPORT_LIMIT,
  NOTE_LENGTH,
  NOTE_LIMIT,
  WORK_ITEM_LIMIT,
  type TurnActivity,
  type WorkItem,
  type WorkSummary,
} from "@carillon/protocol";
import { isAbsolute, relative, resolve } from "node:path";
import { bounded, redactCredentials } from "./failure.ts";

const DETAIL_LIMIT = 120;
const HEADLINE_LIMIT = 100;
const TOOL_NAME_LIMIT = 64;
/** A first line longer than this is treated as a pasted script: only its program is shown. */
const COMMAND_LINE_LIMIT = 400;
/** A quoted argument longer than this is treated as an inline script or payload. */
const QUOTED_LIMIT = 60;

/** What a tool call acts on, or undefined when nothing about it is known to be displayable. */
export function describe(tool: string, args: unknown, workspace: string): string | undefined {
  const fields = isRecord(args) ? args : {};
  switch (tool) {
    case "bash":
    case "powershell":
      return commandHeader(fields["command"]);
    case "read":
    case "write":
    case "edit":
    case "ls":
      return typeof fields["path"] === "string"
        ? displayed(workspacePath(fields["path"], workspace))
        : undefined;
    case "grep":
    case "find": {
      const pattern = fields["pattern"];
      if (typeof pattern !== "string") return undefined;
      const path = fields["path"];
      const quoted = tool === "grep" ? `"${pattern}"` : pattern;
      return displayed(
        typeof path === "string" ? `${quoted} in ${workspacePath(path, workspace)}` : quoted,
      );
    }
    default:
      return operationTarget(fields["operation"]);
  }
}

/**
 * The command's first line: never a later line, a heredoc or here-string, a
 * command or process substitution, the program given to an interpreter,
 * a long quoted argument, anything after a credential-named option, or a
 * quoted argument the redaction had to change. Such cuts are marked with
 * ` …`, as is any omitted later line.
 *
 * The whole command is redacted before anything is cut, with the spaces of
 * escaped words and substitutions kept inside them, so no cut can split a
 * value into a part the redaction hides and a part it misses.
 *
 * This is a display boundary, not a sanitiser: a short secret passed as an
 * ordinary argument, or one the redaction does not recognise, is shown.
 */
export function commandHeader(command: unknown): string | undefined {
  if (typeof command !== "string") return undefined;
  const lines = redactCredentials(withWordSpaces(command)).split(/\r\n|\r|\n/u);
  const index = lines.findIndex((line) => line.trim().length > 0);
  const first = lines[index]?.trim();
  if (first === undefined || first.startsWith("#")) return undefined;
  let omitted = lines.slice(index + 1).some((line) => line.trim().length > 0);
  let header = first;
  if (header.length > COMMAND_LINE_LIMIT) {
    header = header.split(/\s/u, 1)[0] ?? "";
    omitted = true;
  }
  const cut = opaqueStart(header);
  if (cut !== undefined) {
    header = header.slice(0, cut);
    omitted = true;
  }
  const shown = displayed(header.replaceAll(ESCAPED_SPACE, "\\ ").replaceAll(INNER_SPACE, " "));
  if (shown === undefined) return omitted ? "…" : undefined;
  return omitted && !shown.endsWith("…") ? `${shown} …` : shown;
}

/** Stand for a backslash-escaped space and a space inside `$(…)` or backticks. */
const ESCAPED_SPACE = "\uE000";
const INNER_SPACE = "\uE001";

/** Keeps each escaped word and innermost substitution one word for the redaction. */
function withWordSpaces(command: string): string {
  return command
    .replace(/\\[ \t]/gu, ESCAPED_SPACE)
    .replace(/\$\([^()\n]*\)|`[^`\n]*`/gu, (inner) => inner.replace(/[ \t]/gu, INNER_SPACE));
}

const SUBSTITUTION = /<<|\$\(|`|[<>]\(/u;
// An interpreter given its program as an argument, attached or not:
// `python3 -c …`, `python3 -c"…"`, `bash -lc …`, `perl -ne …`.
const INLINE_SCRIPT =
  /(?:^|[\s;&|(])(?:[^\s;&|()]*\/)?(?:python[0-9.]*|node|bun|deno|perl|ruby|php|lua|Rscript|osascript|pwsh|powershell|bash|sh|zsh|dash|ksh|fish)(?:\s+[^\s;&|]+)*?\s+(?:-[A-Za-z]*[ceE]|--eval|--exec|--command|-Command)/u;
const LONG_QUOTED = new RegExp(String.raw`(["'])(?:(?!\1).){${String(QUOTED_LIMIT + 1)},}`, "u");
// Options whose value is a credential, in either form: `--password x`, `--api-key=x`.
const SECRET_OPTION =
  /(?:^|\s)--?(?:[a-z0-9]+-)*(?:pass|password|passwd|passphrase|token|secret|api-?key|key|auth|credentials?|cookie)(?=[=\s]|$)/iu;
// `curl -u user:password`, but not `systemctl --user restart`.
const USER_OPTION = /(?:^|\s)(?:-u|--user)(?=(?:=|\s+)\S*:)/u;
// A quoted argument part of which was redacted: its other words may belong to the same value.
const REDACTED_QUOTE = /(["'])(?:(?!\1).)*?\[redacted\]/u;

/** Where the part of a command line that may be a script, payload or credential begins. */
function opaqueStart(header: string): number | undefined {
  const starts = [
    header.search(SUBSTITUTION),
    header.search(LONG_QUOTED),
    header.search(REDACTED_QUOTE),
    ...[INLINE_SCRIPT, SECRET_OPTION, USER_OPTION]
      .map((pattern) => pattern.exec(header))
      .map((match) => (match === null ? -1 : match.index + match[0].length)),
  ].filter((index) => index !== -1);
  return starts.length === 0 ? undefined : Math.min(...starts);
}

/** An operation-shaped argument's action and numeric target IDs, such as Discord's. */
function operationTarget(operation: unknown): string | undefined {
  if (!isRecord(operation)) return undefined;
  const action = operation["action"];
  if (typeof action !== "string" || !/^[a-z][a-z0-9_.-]{0,63}$/iu.test(action)) return undefined;
  const targets = Object.entries(operation).flatMap(([key, value]) =>
    key.endsWith("_id") && typeof value === "string" && /^[0-9]{1,20}$/u.test(value)
      ? [`${key.slice(0, -3)} ${value}`]
      : [],
  );
  return displayed([action, ...targets].join(" "));
}

/** The first line of the model's public text, as a short headline. */
export function commentary(text: string): string | undefined {
  const line = text.split(/\r\n|\r|\n/u).find((candidate) => candidate.trim().length > 0);
  return line === undefined ? undefined : displayed(line, HEADLINE_LIMIT);
}

function workspacePath(path: string, workspace: string): string {
  const inside = relative(workspace, resolve(workspace, path));
  return inside.length === 0 || inside.startsWith("..") || isAbsolute(inside) ? path : inside;
}

/** A tool's name on one line and bounded, or `tool` when nothing remains. */
export function toolName(name: string): string {
  return bounded(singleLine(name), TOOL_NAME_LIMIT) || "tool";
}

/** Redacted, on one line and bounded; undefined when nothing remains. */
export function displayed(text: string, limit = DETAIL_LIMIT): string | undefined {
  const line = singleLine(redactCredentials(text));
  return line.length === 0 ? undefined : bounded(line, limit);
}

function singleLine(text: string): string {
  return text
    .replace(/\p{Cc}/gu, " ")
    .replace(/\s+/gu, " ")
    .trim();
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

const LIVE_WORK: ReadonlySet<WorkItem["state"]> = new Set(["running", "waiting", "queued"]);

/**
 * A turn's work as one bounded summary: live work first, then work whose
 * state is no longer known, then finished work, each in the runtime's order.
 * Items beyond the snapshot's limit are counted exactly in `omitted`, since
 * the runtime holds them; `partial` passes on what the runtime could not
 * observe at all.
 */
export function summarizeWork(items: readonly WorkItem[], partial: boolean): WorkSummary {
  const rank = (item: WorkItem) =>
    LIVE_WORK.has(item.state) ? 0 : item.state === "unknown" ? 1 : 2;
  const ordered = items.toSorted((left, right) => rank(left) - rank(right));
  const omitted = Math.max(0, ordered.length - WORK_ITEM_LIMIT);
  return {
    items: ordered.slice(0, WORK_ITEM_LIMIT),
    partial,
    ...(omitted > 0 ? { omitted } : {}),
  };
}

/**
 * Keeps reasoning text the engine published as the note of its native
 * identity `id`: redacted, on one line and bounded, from its beginning. A
 * note already kept is updated in place, so streamed text never reorders
 * the notes; beyond {@link NOTE_LIMIT}, the oldest go.
 */
export function keepNote(notes: Map<string, string>, id: string, text: string): void {
  const note = displayed(text, NOTE_LENGTH);
  if (note === undefined) return;
  notes.set(id, note);
  for (const oldest of notes.keys()) {
    if (notes.size <= NOTE_LIMIT) break;
    notes.delete(oldest);
  }
}

/**
 * A turn's final report within {@link ACTIVITY_REPORT_LIMIT}, which bounds
 * its serialized size whatever its text holds. Every part is already bounded
 * in count; when their text still adds up past the limit, parts go in order
 * until it fits, so live state stays longest: the oldest notes, finished work,
 * the oldest finished calls, the headline, then live work and the latest
 * running calls. Work that goes is counted as omitted, and a report that
 * lost anything says it is `partial`.
 */
export function finalReport(activity: TurnActivity): TurnActivity {
  const { notes = [], work, recent, running, headline, partial = false, ...rest } = activity;
  let [kept, items, omitted, calls, live, line, cut] = [
    notes,
    work?.items ?? [],
    work?.omitted ?? 0,
    recent,
    running,
    headline,
    partial,
  ];
  const compose = (): TurnActivity => ({
    ...rest,
    ...(line === undefined ? {} : { headline: line }),
    running: live,
    recent: calls,
    ...(work === undefined
      ? {}
      : { work: { items, partial: work.partial, ...(omitted > 0 ? { omitted } : {}) } }),
    ...(kept.length > 0 ? { notes: kept } : {}),
    ...(cut ? { partial: true } : {}),
  });
  let report = compose();
  while (Buffer.byteLength(JSON.stringify(report)) > ACTIVITY_REPORT_LIMIT) {
    const last = items.at(-1);
    if (kept.length > 0) kept = kept.slice(1);
    else if (last !== undefined && !LIVE_WORK.has(last.state))
      [items, omitted] = [items.slice(0, -1), omitted + 1];
    else if (calls.length > 0) calls = calls.slice(1);
    else if (line !== undefined) line = undefined;
    else if (items.length > 0) [items, omitted] = [items.slice(0, -1), omitted + 1];
    else if (live.length > 0) live = live.slice(0, -1);
    // The phase, the counts and the agent's bounded names always fit.
    else break;
    cut = true;
    report = compose();
  }
  return report;
}
