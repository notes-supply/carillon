import { EventSourceParserStream } from "eventsource-parser/stream";
import { Type, type Static, type TProperties } from "typebox";
import { Check } from "typebox/value";

function closed<Properties extends TProperties>(properties: Properties) {
  return Type.Object(properties, { additionalProperties: false });
}

const UUID = "[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}";
export const AUDIENCE_ID_PATTERN = `^${UUID}$`;
export const EVENT_ID_PATTERN = `^m-${UUID}$`;
export const WINDOW_LIMIT = 500;

const UnixSeconds = Type.Integer({ minimum: 0 });
const Count = Type.Integer({ minimum: 0 });
const Words = Type.String({ minLength: 1 });
const Present = Type.Optional(Type.Literal(true));

const AgentSchema = closed({ id: Words, name: Words });
export type Agent = Static<typeof AgentSchema>;

const LimitsSchema = closed({
  files: Count,
  file_bytes: Count,
  message_bytes: Count,
});
export type Limits = Static<typeof LimitsSchema>;

export const ConversationKeySchema = closed({
  agent_id: Words,
  audience_id: Type.String({ pattern: AUDIENCE_ID_PATTERN }),
});
export type ConversationKey = Static<typeof ConversationKeySchema>;

const ConversationSchema = closed({
  key: ConversationKeySchema,
  title: Type.String(),
  started_at: UnixSeconds,
  active_at: UnixSeconds,
  activity: Type.Integer(),
  at_work: Count,
});
export type Conversation = Static<typeof ConversationSchema>;

const FileLinkSchema = closed({
  name: Words,
  size: Count,
  type: Type.Union([
    Type.Literal("image/png"),
    Type.Literal("image/jpeg"),
    Type.Literal("image/gif"),
    Type.Literal("image/webp"),
    Type.Literal("application/octet-stream"),
  ]),
  url: Words,
});
export type FileLink = Static<typeof FileLinkSchema>;

const MessageSourceSchema = closed({
  kind: Type.Literal("message"),
  event_id: Type.String({ pattern: EVENT_ID_PATTERN }),
  text: Type.String(),
  command: Type.Boolean(),
  files: Type.Array(FileLinkSchema),
});
export type MessageSource = Static<typeof MessageSourceSchema>;

const OpenedSourceSchema = closed({
  kind: Type.Literal("opened"),
  label: Type.Optional(Words),
});

const ActivityPhaseSchema = Type.Union([
  Type.Literal("starting"),
  Type.Literal("queued"),
  Type.Literal("model"),
  Type.Literal("tools"),
  Type.Literal("waiting"),
  Type.Literal("retrying"),
  Type.Literal("compacting"),
]);
export type ActivityPhase = Static<typeof ActivityPhaseSchema>;

const RunningToolSchema = closed({
  tool: Words,
  detail: Type.Optional(Words),
  started_at: UnixSeconds,
  estimated: Present,
});

const FinishedToolSchema = closed({
  tool: Words,
  detail: Type.Optional(Words),
  ok: Type.Boolean(),
  duration_ms: Type.Optional(Count),
});
export type FinishedTool = Static<typeof FinishedToolSchema>;

const WorkItemSchema = closed({
  id: Words,
  kind: Type.Union([
    Type.Literal("task"),
    Type.Literal("delegation"),
    Type.Literal("handoff"),
    Type.Literal("fork"),
  ]),
  label: Words,
  state: Type.Union([
    Type.Literal("queued"),
    Type.Literal("running"),
    Type.Literal("waiting"),
    Type.Literal("succeeded"),
    Type.Literal("failed"),
    Type.Literal("cancelled"),
    Type.Literal("unknown"),
  ]),
  parent: Type.Optional(Words),
  actor: Type.Optional(Words),
  started_at: Type.Optional(UnixSeconds),
  estimated: Present,
  duration_ms: Type.Optional(Count),
});
export type WorkItem = Static<typeof WorkItemSchema>;

const WorkSchema = closed({
  items: Type.Array(WorkItemSchema, { maxItems: 16 }),
  partial: Type.Boolean(),
  omitted: Type.Optional(Type.Integer({ minimum: 1 })),
});

const AgentInfoSchema = closed({
  model: Type.Optional(Words),
  effort: Type.Optional(Words),
  context: Type.Optional(closed({ used: Count, window: Type.Optional(Count) })),
});

const ActivitySchema = closed({
  phase: ActivityPhaseSchema,
  headline: Type.Optional(Words),
  running: Type.Array(RunningToolSchema),
  recent: Type.Array(FinishedToolSchema),
  tool_calls: Count,
  work: Type.Optional(WorkSchema),
  notes: Type.Optional(Type.Array(Type.String({ minLength: 1, maxLength: 500 }), { maxItems: 8 })),
  agent: Type.Optional(AgentInfoSchema),
  partial: Present,
});
export type Activity = Static<typeof ActivitySchema>;

export const ControlSchema = Type.Union([
  Type.Literal("steer"),
  Type.Literal("interrupt"),
  Type.Literal("stop"),
]);
export type Control = Static<typeof ControlSchema>;

const EndingSchema = Type.Union([
  closed({
    kind: Type.Literal("answered"),
    html: Type.String(),
    files: Type.Array(FileLinkSchema),
  }),
  closed({ kind: Type.Literal("interrupted"), html: Type.String() }),
  closed({ kind: Type.Literal("steered") }),
  closed({ kind: Type.Literal("stopped") }),
]);
export type Ending = Static<typeof EndingSchema>;

const ProgressSchema = Type.Union([
  closed({ state: Type.Literal("queued") }),
  closed({
    state: Type.Literal("running"),
    activity: Type.Union([ActivitySchema, Type.Null()]),
    controls: Type.Array(ControlSchema, { maxItems: 3, uniqueItems: true }),
  }),
  closed({
    state: Type.Literal("ended"),
    finished_at: UnixSeconds,
    ending: EndingSchema,
    report: Type.Optional(ActivitySchema),
  }),
]);
export type Progress = Static<typeof ProgressSchema>;

const TurnSchema = closed({
  turn_id: Words,
  source: Type.Union([MessageSourceSchema, OpenedSourceSchema]),
  accepted_at: UnixSeconds,
  progress: ProgressSchema,
});
export type Turn = Static<typeof TurnSchema>;

const OpenedSchema = Type.Union([
  closed({ kind: Type.Literal("unrecorded"), key: ConversationKeySchema }),
  closed({
    kind: Type.Literal("recorded"),
    conversation: ConversationSchema,
    turns: Type.Array(TurnSchema, { maxItems: WINDOW_LIMIT }),
    earlier: Type.Boolean(),
  }),
]);
export type Opened = Static<typeof OpenedSchema>;

const ChatSnapshotSchema = closed({
  agents: Type.Array(AgentSchema),
  limits: LimitsSchema,
  custom_site: Type.Boolean(),
  conversations: Type.Array(ConversationSchema, { maxItems: WINDOW_LIMIT }),
  more: Type.Boolean(),
  open: Type.Union([OpenedSchema, Type.Null()]),
});
export type ChatSnapshot = Static<typeof ChatSnapshotSchema>;

const StreamMessageSchema = Type.Union([
  closed({ kind: Type.Literal("snapshot"), snapshot: ChatSnapshotSchema }),
  closed({ kind: Type.Literal("unavailable") }),
  closed({ kind: Type.Literal("signed_out") }),
]);
export type StreamMessage = Static<typeof StreamMessageSchema>;

export type Received = StreamMessage | { kind: "unreadable" };

export function parsedJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

export function readStreamMessage(data: string): Received {
  const message = parsedJson(data);
  return Check(StreamMessageSchema, message) ? message : { kind: "unreadable" };
}

export async function* streamMessages(
  body: ReadableStream<Uint8Array<ArrayBuffer>>,
): AsyncGenerator<Received, void, void> {
  const reader = body
    .pipeThrough(new TextDecoderStream())
    .pipeThrough(new EventSourceParserStream())
    .getReader();
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return;
      yield readStreamMessage(value.data);
    }
  } finally {
    reader.releaseLock();
  }
}
