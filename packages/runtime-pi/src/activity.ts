/**
 * What an executing turn is doing, derived from native Pi session events and
 * bounded for display. Tool inputs are described here, beside Pi's tool
 * semantics, so no caller ever receives raw tool arguments.
 *
 * Descriptions are deny-by-default: a known tool shows the field that says
 * what it acts on, a tool shaped like an operation shows its action and
 * numeric target IDs, and anything else shows only its name. Every detail
 * passes the same best-effort credential redaction as diagnostics before it
 * is cut to length.
 */
import type { AgentSessionEvent } from "@earendil-works/pi-coding-agent";
import type { ActivityPhase, FinishedTool, RunningTool, TurnActivity } from "@carillon/protocol";
import { isAbsolute, relative, resolve } from "node:path";
import { bounded, redactCredentials } from "./failure.ts";

/** The most finished calls a snapshot lists; older calls are only counted. */
const RECENT_CALLS = 5;
/** The most running calls a snapshot lists, in the order they started. */
const RUNNING_CALLS = 8;
const DETAIL_LIMIT = 120;
const HEADLINE_LIMIT = 100;
const TOOL_NAME_LIMIT = 64;
/** A first line longer than this is treated as a pasted script: only its program is shown. */
const COMMAND_LINE_LIMIT = 400;
/** A quoted argument longer than this is treated as an inline script or payload. */
const QUOTED_LIMIT = 60;

export interface ActivityTracker {
  observe(event: AgentSessionEvent): void;
  snapshot(): TurnActivity;
}

/** Tracks one turn from before its session exists until it settles. */
export function trackActivity(workspace: string): ActivityTracker {
  let phase: ActivityPhase = "starting";
  let headline: string | undefined;
  let toolCalls = 0;
  const running = new Map<string, RunningTool>();
  const recent: FinishedTool[] = [];
  const idle = (): ActivityPhase => (running.size > 0 ? "tools" : "model");

  return {
    observe(event) {
      switch (event.type) {
        case "agent_start":
        case "auto_retry_end":
        case "compaction_end":
          phase = idle();
          break;
        case "auto_retry_start":
          phase = "retrying";
          break;
        case "compaction_start":
          phase = "compacting";
          break;
        case "message_update":
          // Only completed public text: thinking arrives as its own events.
          if (event.assistantMessageEvent.type === "text_end")
            headline = commentary(event.assistantMessageEvent.content) ?? headline;
          break;
        case "tool_execution_start": {
          toolCalls += 1;
          const detail = describe(event.toolName, event.args, workspace);
          running.set(event.toolCallId, {
            tool: toolName(event.toolName),
            ...(detail === undefined ? {} : { detail }),
            started_at: Math.floor(Date.now() / 1000),
          });
          phase = "tools";
          break;
        }
        case "tool_execution_end": {
          const started = running.get(event.toolCallId);
          running.delete(event.toolCallId);
          recent.push({
            tool: started?.tool ?? toolName(event.toolName),
            ...(started?.detail === undefined ? {} : { detail: started.detail }),
            ok: !event.isError,
          });
          recent.splice(0, Math.max(0, recent.length - RECENT_CALLS));
          phase = idle();
          break;
        }
        // Starts, ends and deltas of turns and messages say nothing a card
        // shows; tool output is never described.
        case "agent_end":
        case "agent_settled":
        case "turn_start":
        case "turn_end":
        case "message_start":
        case "message_end":
        case "tool_execution_update":
        case "bash_execution_update":
        case "queue_update":
        case "entry_appended":
        case "session_info_changed":
        case "thinking_level_changed":
        case "summarization_retry_scheduled":
        case "summarization_retry_attempt_start":
        case "summarization_retry_finished":
          break;
      }
    },
    snapshot() {
      return {
        phase,
        ...(headline === undefined ? {} : { headline }),
        running: [...running.values()].slice(0, RUNNING_CALLS),
        recent: [...recent],
        tool_calls: toolCalls,
      };
    },
  };
}

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
 * command or process substitution, the script after an inline-script flag,
 * or a long quoted argument. Such cuts are marked with ` …`, as is any
 * omitted later line. These cuts fall on shell syntax, never inside a word,
 * so what remains is redacted whole, with values after credential-named
 * options, before anything is cut for length.
 *
 * This is a display boundary, not a sanitiser: a short secret passed as an
 * ordinary argument, or one the redaction does not recognise, is shown.
 */
export function commandHeader(command: unknown): string | undefined {
  if (typeof command !== "string") return undefined;
  const lines = command.split(/\r\n|\r|\n/u);
  const index = lines.findIndex((line) => line.trim().length > 0);
  const first = lines[index]?.trim();
  if (first === undefined || first.startsWith("#")) return undefined;
  let omitted = lines.slice(index + 1).some((line) => line.trim().length > 0);
  let header = withoutOptionSecrets(first);
  if (first.length > COMMAND_LINE_LIMIT) {
    header = header.split(/\s/u, 1)[0] ?? "";
    omitted = true;
  }
  const cut = opaqueStart(header);
  if (cut !== undefined) {
    header = header.slice(0, cut);
    omitted = true;
  }
  const shown = displayed(header);
  if (shown === undefined) return omitted ? "…" : undefined;
  return omitted && !shown.endsWith("…") ? `${shown} …` : shown;
}

const SUBSTITUTION = /<<|\$\(|`|[<>]\(/u;
// An interpreter given its program as an argument: `python3 -c …`, `bash -lc …`, `perl -ne …`.
const INLINE_SCRIPT =
  /(?:^|[\s;&|(])(?:[^\s;&|()]*\/)?(?:python[0-9.]*|node|bun|deno|perl|ruby|php|lua|Rscript|osascript|pwsh|powershell|bash|sh|zsh|dash|ksh|fish)(?:\s+[^\s;&|]+)*?\s+(?:-[A-Za-z]*[ceE]|--eval|--exec|--command|-Command)(?=\s|$)/u;
const LONG_QUOTED = new RegExp(String.raw`(["'])(?:(?!\1).){${String(QUOTED_LIMIT + 1)},}`, "u");

/** Where the part of a command line that may be a script or payload begins. */
function opaqueStart(header: string): number | undefined {
  const starts = [
    header.search(SUBSTITUTION),
    header.search(LONG_QUOTED),
    ...[INLINE_SCRIPT.exec(header)].map((flag) =>
      flag === null ? -1 : flag.index + flag[0].length,
    ),
  ].filter((index) => index !== -1);
  return starts.length === 0 ? undefined : Math.min(...starts);
}

// Options whose next argument is a credential: `--password x`, `--api-key x`.
const SECRET_OPTION =
  /^--?(?:[a-z0-9]+-)*(?:pass|password|passwd|passphrase|token|secret|api-?key|key|auth|credentials?|cookie)$/iu;
// `curl -u user:password`.
const USER_OPTION = /^(?:-u|--user)$/u;

function withoutOptionSecrets(text: string): string {
  const words = text.split(/(\s+)/u);
  for (let index = 2; index < words.length; index += 2) {
    const option = words[index - 2] ?? "";
    const value = words[index] ?? "";
    if (SECRET_OPTION.test(option) || (USER_OPTION.test(option) && value.includes(":")))
      words[index] = "[redacted]";
  }
  return words.join("");
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
function commentary(text: string): string | undefined {
  const line = text.split(/\r\n|\r|\n/u).find((candidate) => candidate.trim().length > 0);
  return line === undefined ? undefined : displayed(line, HEADLINE_LIMIT);
}

function workspacePath(path: string, workspace: string): string {
  const inside = relative(workspace, resolve(workspace, path));
  return inside.length === 0 || inside.startsWith("..") || isAbsolute(inside) ? path : inside;
}

function toolName(name: string): string {
  return bounded(singleLine(name), TOOL_NAME_LIMIT) || "tool";
}

/** Redacted, on one line and bounded; undefined when nothing remains. */
function displayed(text: string, limit = DETAIL_LIMIT): string | undefined {
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
