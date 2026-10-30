/**
 * Schedules: prompts a conversation runs later, once or on a cron
 * expression, each run a turn the runtime opens. A schedule is a background
 * task of its conversation that sleeps until it is due, so a restart
 * resumes it; runs missed while the runtime was down run once. It waits for
 * each run's opening before it is due again, so one run at a time is under
 * way. Deselecting `schedules` ends each schedule when it is next due, and
 * stops a run whose turn is still opening.
 */
import type { Context, JsonValue } from "@earendil-works/chord";
import {
  defineDocFamily,
  defineExtension,
  defineTask,
  defineTool,
  type Extension,
  type TaskId,
  type TaskRecord,
  type ToolExecutionApi,
} from "@earendil-works/pi-durable";
import { Cron } from "croner";
import { Type, type Static } from "typebox";
import { Check } from "typebox/value";
import { backoff } from "./delivery.ts";
import type { OpeningTask } from "./opening.ts";

export const SCHEDULES = "schedules";
export const SCHEDULE = "carillon.schedule";

const ScheduleInputSchema = Type.Object({
  prompt: Type.String(),
  name: Type.Optional(Type.String()),
  /** When it is first due, on the harness clock. */
  at: Type.Number(),
  /** When it is due again; without it, the schedule runs once. */
  cron: Type.Optional(Type.String()),
  /** The IANA time zone of `cron`. */
  timezone: Type.String(),
});
type ScheduleInput = Static<typeof ScheduleInputSchema>;

const ScheduleStateSchema = Type.Union([
  /** The run due at `at`, counted from 1, and how often it found the selection unknown. */
  Type.Object({
    phase: Type.Literal("wait"),
    run: Type.Integer(),
    at: Type.Number(),
    attempt: Type.Integer(),
  }),
  /** The run whose opening was under way, done once this phase runs. */
  Type.Object({ phase: Type.Literal("ran"), run: Type.Integer() }),
]);
type ScheduleState = Static<typeof ScheduleStateSchema>;

export interface ScheduleHost {
  /** Opens each run's turn. */
  readonly opening: OpeningTask;
  /** Whether the agent selects the extension; undefined while that is unknown. */
  selects(extension: string): boolean | undefined;
  /** Ends a schedule's task. */
  cancel(task: TaskId): Promise<void>;
}

/** Each schedule call, by its tool task: the schedule it made. */
const Calls = defineDocFamily<{ schedule?: { id: TaskId; at: number } }, null>({
  kind: "carillon.schedule-call",
  version: 1,
  scope: "session",
  family: true,
  initial: () => ({}),
  checkpointWhen: () => true,
});

const LIVE = new Set(["pending", "running", "waiting", "sleeping"]);
const ZONE = Intl.DateTimeFormat().resolvedOptions().timeZone;
const COMPLETED = { status: "terminal", outcome: { status: "completed", result: null } } as const;
const ISO_TIME = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:\d{2})?$/u;

export function createSchedulesExtension(host: ScheduleHost): Extension {
  const schedule = defineTask<ScheduleInput, ScheduleState, null>({
    name: SCHEDULE,
    version: 1,
    initial: (input) => ({ phase: "wait", run: 1, at: input.at, attempt: 0 }),
    phases: {
      wait: async (task, runtime, context) => {
        const { run, at, attempt } = task.state.checkpoint;
        if (runtime.now() < at) {
          await runtime.sleepUntil(at, task.state.checkpoint, context);
          return;
        }
        const selected = host.selects(SCHEDULES);
        // Resources that no longer load leave it unknown until the next that do.
        if (selected === undefined) {
          const delay = backoff(attempt);
          console.error(
            `schedule ${String(task.id)}: the agent's extensions are unknown; looking again in ${String(delay / 1_000)} s`,
          );
          const checkpoint = { ...task.state.checkpoint, attempt: attempt + 1 };
          await runtime.sleepUntil(runtime.now() + delay, checkpoint, context);
          return;
        }
        if (!selected) {
          await runtime.commit(() => COMPLETED, context);
          return;
        }
        await runtime.commit(async (tx) => {
          const opening = await tx.createTask(
            host.opening,
            {
              key: `schedule:${String(task.id)}:${String(run)}`,
              label: `Scheduled: ${task.input.name ?? task.input.prompt}`,
              content: [
                {
                  type: "text",
                  text: `[Carillon] This is a scheduled run${task.input.name === undefined ? "" : ` of "${task.input.name}"`}, not a message someone sent now. Your answer is posted to the conversation.`,
                },
                { type: "text", text: task.input.prompt },
              ],
              needs: { task: task.id, extension: SCHEDULES },
            },
            { ownership: { kind: "conversation" }, background: true },
          );
          return {
            status: "waiting",
            on: [opening],
            policy: "allSettled",
            checkpoint: { phase: "ran", run },
          };
        }, context);
      },
      // Due again only from now, so runs an outage held up do not follow at once.
      ran: async (task, runtime, context) => {
        const { run } = task.state.checkpoint;
        const next = following(task.input, runtime.now());
        await runtime.commit(
          () =>
            next === undefined
              ? COMPLETED
              : {
                  status: "sleeping",
                  until: next,
                  checkpoint: { phase: "wait", run: run + 1, at: next, attempt: 0 },
                },
          context,
        );
      },
    },
    abort: (_task, runtime, context) =>
      runtime.commit(() => ({ status: "terminal", outcome: { status: "aborted" } }), context),
  });

  return defineExtension({
    name: SCHEDULES,
    tasks: [schedule],
    tools: [
      defineTool({
        name: "schedule",
        description:
          "Runs a prompt later in this conversation: once at a time, or repeatedly on a cron expression. Each run is a message of its own to you, and your answer to it is posted to the conversation. Returns the schedule's ID and when it first runs.",
        parameters: Type.Object(
          {
            prompt: Type.String({
              minLength: 1,
              description: "What to do when it runs, written as a message to yourself.",
            }),
            at: Type.Optional(
              Type.String({
                description:
                  "When to run once: an ISO 8601 time, such as 2026-10-25T09:00:00+02:00. Without an offset, it is in `timezone`.",
              }),
            ),
            cron: Type.Optional(
              Type.String({
                description:
                  "When to run repeatedly: a cron expression of five fields, minute, hour, day of month, month and day of week, in `timezone`.",
              }),
            ),
            timezone: Type.Optional(
              Type.String({ description: `An IANA time zone. Default: ${ZONE}.` }),
            ),
            name: Type.Optional(
              Type.String({ minLength: 1, description: "A short name to tell it apart." }),
            ),
          },
          { additionalProperties: false },
        ),
        // A rerun after a crash finds the schedule its call made, though its time may have passed.
        replay: "safe",
        async execute({ prompt, at, cron, timezone = ZONE, name }, api, context) {
          const found = (await api.snapshot(Calls, String(api.taskId), context))?.schedule;
          if (found !== undefined) return scheduled(found);
          const pattern = at ?? cron;
          if (pattern === undefined || (at !== undefined && cron !== undefined))
            throw new Error("Give exactly one of at or cron.");
          if (at !== undefined && !ISO_TIME.test(at))
            throw new Error(`at must be an ISO 8601 time, such as 2026-10-25T09:00:00+02:00.`);
          // Five fields run at most once a minute.
          if (cron !== undefined && cron.trim().split(/\s+/u).length !== 5)
            throw new Error(
              "cron must have five fields: minute, hour, day of month, month and day of week.",
            );
          const now = api.now();
          let first: Date | null;
          try {
            first = new Cron(pattern, { timezone }).nextRun(new Date(now));
          } catch (failure) {
            throw new Error(
              `${at === undefined ? "cron" : "at"} cannot be used: ${failure instanceof Error ? failure.message : String(failure)}`,
              { cause: failure },
            );
          }
          if (first === null)
            throw new Error(`${pattern} is never due after now, ${new Date(now).toISOString()}.`);
          const input: ScheduleInput = {
            prompt,
            at: first.getTime(),
            timezone,
            ...(name === undefined ? {} : { name }),
            ...(cron === undefined ? {} : { cron }),
          };
          const made = await api.commit(async (tx) => {
            const call = await tx.doc(Calls, String(api.taskId), null);
            call.schedule ??= {
              id: await tx.createTask(schedule, input, {
                ownership: { kind: "conversation" },
                background: true,
              }),
              at: input.at,
            };
            return { ...call.schedule };
          }, context);
          return scheduled(made);
        },
      }),
      defineTool({
        name: "schedule_list",
        description: "Lists this conversation's schedules: their IDs, prompts and next runs.",
        parameters: Type.Object({}),
        async execute(_args, api, context) {
          const schedules = await live(api, context);
          return text(
            schedules.length === 0
              ? "This conversation has no schedules."
              : schedules.map(describe).join("\n"),
          );
        },
      }),
      defineTool({
        name: "schedule_cancel",
        description: "Cancels one of this conversation's schedules by its ID.",
        parameters: Type.Object({ id: Type.Integer() }, { additionalProperties: false }),
        async execute({ id }, api, context) {
          const found = (await live(api, context)).find((schedule) => schedule.id === id);
          if (found === undefined)
            throw new Error(`This conversation has no schedule ${String(id)}.`);
          await host.cancel(found.id);
          return text(`Cancelled schedule ${String(id)}.`);
        },
      }),
    ],
  });
}

interface LiveSchedule {
  readonly id: TaskId;
  readonly input: ScheduleInput;
  readonly state: ScheduleState;
}

/** The calling conversation's schedules that may still run, as its task records hold them. */
function live(api: ToolExecutionApi, context: Context): Promise<LiveSchedule[]> {
  const { conversationId } = api;
  return api.commit(async (tx) => {
    const found: LiveSchedule[] = [];
    let cursor;
    do {
      const page = await tx.scanTasks({ conversationId, kind: SCHEDULE }, 100, cursor);
      for (const record of page.items) {
        const schedule = liveSchedule(record);
        if (schedule !== undefined) found.push(schedule);
      }
      cursor = page.next;
    } while (cursor !== undefined);
    return found;
  }, context);
}

function liveSchedule(
  record: TaskRecord<JsonValue, JsonValue, JsonValue>,
): LiveSchedule | undefined {
  const { id, input, state } = record;
  if (!LIVE.has(state.status) || record.abortRequested) return undefined;
  return Check(ScheduleInputSchema, input) && Check(ScheduleStateSchema, state.checkpoint)
    ? { id, input, state: state.checkpoint }
    : undefined;
}

function describe({ id, input, state }: LiveSchedule): string {
  const name = input.name === undefined ? "" : ` "${input.name}"`;
  const when =
    state.phase === "wait" ? `next at ${new Date(state.at).toISOString()}` : "running now";
  const repeat = input.cron === undefined ? "once" : `then on ${input.cron} in ${input.timezone}`;
  return `${String(id)}${name}: ${when}, ${repeat}: ${input.prompt}`;
}

/** When a recurring schedule is next due after `after`; undefined once it is done. */
function following(input: ScheduleInput, after: number): number | undefined {
  if (input.cron === undefined) return undefined;
  return new Cron(input.cron, { timezone: input.timezone }).nextRun(new Date(after))?.getTime();
}

function scheduled({ id, at }: { readonly id: TaskId; readonly at: number }) {
  return text(`Schedule ${String(id)} first runs at ${new Date(at).toISOString()}.`);
}

function text(value: string) {
  return { output: [{ type: "text" as const, text: value }] };
}
