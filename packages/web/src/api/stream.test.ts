import { expect, test } from "vitest";
import {
  readStreamMessage,
  streamMessages,
  type Activity,
  type ChatSnapshot,
  type Conversation,
  type Received,
  type Turn,
} from "./stream.ts";
import sample from "./stream.sample.json" with { type: "json" };

const KEY = { agent_id: "assistant", audience_id: "0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b" };
const EVENT = "m-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa";

const conversation: Conversation = {
  key: KEY,
  title: "Plan the release",
  started_at: 1_794_499_000,
  active_at: 1_794_499_146,
  activity: 57,
  at_work: 2,
};

const activity: Activity = {
  phase: "tools",
  headline: "Reading the release notes.",
  running: [
    { tool: "read", detail: "docs/RELEASE.md", started_at: 1_794_499_120, estimated: true },
  ],
  recent: [
    { tool: "bash", detail: "git log -5", ok: true, duration_ms: 41 },
    { tool: "bash", ok: false },
  ],
  tool_calls: 3,
  work: {
    items: [{ id: "w1", kind: "delegation", label: "Check the changelog", state: "running" }],
    partial: false,
    omitted: 2,
  },
  notes: ["The changelog is out of date."],
  agent: { model: "gpt-6.1", effort: "high", context: { used: 18_250, window: 400_000 } },
};

const file = {
  name: "chart.png",
  size: 20_417,
  type: "image/png",
  url: "/files/a/0?sig=x",
} as const;
const message = { kind: "message", event_id: EVENT, text: "Chart it.", command: false } as const;

const turns: Turn[] = [
  {
    turn_id: "t1",
    source: { ...message, files: [{ ...file, type: "application/octet-stream" }] },
    accepted_at: 1_794_499_100,
    progress: {
      state: "ended",
      finished_at: 1_794_499_146,
      ending: { kind: "answered", html: "<p>Here.</p>\n", files: [file] },
      report: { phase: "model", running: [], recent: [], tool_calls: 0 },
    },
  },
  {
    turn_id: "t2",
    source: { kind: "opened", label: "Scheduled run" },
    accepted_at: 1_794_499_150,
    progress: {
      state: "ended",
      finished_at: 1_794_499_151,
      ending: { kind: "interrupted", html: "<p>Core restarted.</p>\n" },
    },
  },
  {
    turn_id: "t3",
    source: { ...message, files: [] },
    accepted_at: 1_794_499_152,
    progress: { state: "ended", finished_at: 1_794_499_153, ending: { kind: "steered" } },
  },
  {
    turn_id: "t4",
    source: { kind: "opened" },
    accepted_at: 1_794_499_154,
    progress: { state: "ended", finished_at: 1_794_499_155, ending: { kind: "stopped" } },
  },
  {
    turn_id: "t5",
    source: { ...message, files: [] },
    accepted_at: 1_794_499_156,
    progress: { state: "running", activity, controls: ["stop"] },
  },
  {
    turn_id: "t6",
    source: { ...message, files: [] },
    accepted_at: 1_794_499_157,
    progress: { state: "running", activity: null, controls: [] },
  },
  {
    turn_id: "t7",
    source: { ...message, files: [] },
    accepted_at: 1_794_499_158,
    progress: { state: "queued" },
  },
];

const snapshot: ChatSnapshot = {
  agents: [{ id: "assistant", name: "Carillon" }],
  limits: { files: 10, file_bytes: 1_073_741_824, message_bytes: 2_147_483_648 },
  custom_site: false,
  conversations: [conversation],
  more: false,
  open: { kind: "recorded", conversation, turns, earlier: true },
};

function sent(value: unknown): Received {
  return readStreamMessage(JSON.stringify(value));
}

function withTurn(turn: unknown): unknown {
  return {
    kind: "snapshot",
    snapshot: {
      ...snapshot,
      open: { kind: "recorded", conversation, turns: [turn], earlier: false },
    },
  };
}

const ended = turns[0];
const running = turns[4];

test("every message the provider sends is read as it was written", () => {
  const messages: unknown[] = [
    { kind: "snapshot", snapshot },
    { kind: "snapshot", snapshot: { ...snapshot, open: null } },
    { kind: "snapshot", snapshot: { ...snapshot, open: { kind: "unrecorded", key: KEY } } },
    { kind: "snapshot", snapshot: { ...snapshot, agents: [], conversations: [], more: true } },
    { kind: "unavailable" },
    { kind: "signed_out" },
  ];
  for (const written of messages) expect(sent(written)).toStrictEqual(written);
});

test("the message the provider's contract test writes is read as the snapshot it is", () => {
  expect(sent(sample)).toStrictEqual(sample);
});

const broken: Record<string, unknown> = {
  "an unknown kind": { kind: "invalidated" },
  "a snapshot message without its snapshot": { kind: "snapshot" },
  "an unavailable message that carries a snapshot": { kind: "unavailable", snapshot },
  "turns outside a conversation": { kind: "snapshot", snapshot: { ...snapshot, turns } },
  "turns in a conversation core has not recorded": {
    kind: "snapshot",
    snapshot: { ...snapshot, open: { kind: "unrecorded", key: KEY, turns } },
  },
  "a recorded conversation without its turns": {
    kind: "snapshot",
    snapshot: { ...snapshot, open: { kind: "recorded", conversation, earlier: false } },
  },
  "a snapshot without the interface's own facts": {
    kind: "snapshot",
    snapshot: { conversations: [], more: false, open: null },
  },
  "live activity on an ended turn": withTurn({
    ...ended,
    progress: { ...ended?.progress, activity },
  }),
  "controls on an ended turn": withTurn({
    ...ended,
    progress: { ...ended?.progress, controls: ["stop"] },
  }),
  "controls inside the activity, where core has them": withTurn({
    ...running,
    progress: { state: "running", activity: { ...activity, controls: ["stop"] }, controls: [] },
  }),
  "a control offered twice": withTurn({
    ...running,
    progress: { state: "running", activity, controls: ["stop", "stop"] },
  }),
  "a running turn that does not say whether it has activity": withTurn({
    ...running,
    progress: { state: "running", controls: [] },
  }),
  "an ending on a queued turn": withTurn({
    ...ended,
    progress: { state: "queued", ending: { kind: "stopped" } },
  }),
  "an ended turn without an ending": withTurn({
    ...ended,
    progress: { state: "ended", finished_at: 1_794_499_146 },
  }),
  "an unknown progress state": withTurn({ ...ended, progress: { state: "completed" } }),
  "an unknown ending": withTurn({
    ...ended,
    progress: { state: "ended", finished_at: 1_794_499_146, ending: { kind: "failed" } },
  }),
  "an answer as the Markdown core keeps": withTurn({
    ...ended,
    progress: {
      state: "ended",
      finished_at: 1_794_499_146,
      ending: { kind: "answered", text: "Here.", files: [] },
    },
  }),
  "a file with its path on the host": withTurn({
    ...ended,
    source: { ...message, files: [{ ...file, path: "state/attachments/a/0" }] },
  }),
  "a file of a type no link answers with": withTurn({
    ...ended,
    source: { ...message, files: [{ ...file, type: "image/svg+xml" }] },
  }),
  "an event ID the page did not mint": withTurn({
    ...ended,
    source: { ...message, event_id: "t1", files: [] },
  }),
  "a turn that began with both a message and the runtime": withTurn({
    ...ended,
    source: { kind: "opened", label: "Scheduled run", text: "Chart it." },
  }),
  "a conversation with core's channel": {
    kind: "snapshot",
    snapshot: { ...snapshot, conversations: [{ ...conversation, channel_id: KEY.audience_id }] },
  },
  "a conversation the page did not make": {
    kind: "snapshot",
    snapshot: {
      ...snapshot,
      conversations: [
        { ...conversation, key: { agent_id: "assistant", audience_id: "discord-1" } },
      ],
    },
  },
  "a conversation without its agent": {
    kind: "snapshot",
    snapshot: {
      ...snapshot,
      conversations: [{ ...conversation, key: { audience_id: KEY.audience_id } }],
    },
  },
  "a start that says it is not estimated": withTurn({
    ...running,
    progress: {
      state: "running",
      activity: { ...activity, running: [{ tool: "read", started_at: 1, estimated: false }] },
      controls: [],
    },
  }),
  "a time that is not whole seconds": withTurn({ ...ended, accepted_at: 1_794_499_100.5 }),
  "a list": [{ kind: "unavailable" }],
  nothing: null,
};

for (const [name, written] of Object.entries(broken)) {
  test(`${name} is unreadable`, () => {
    expect(sent(written)).toStrictEqual({ kind: "unreadable" });
  });
}

test("text that is not JSON is unreadable", () => {
  expect(readStreamMessage("{")).toStrictEqual({ kind: "unreadable" });
  expect(readStreamMessage("")).toStrictEqual({ kind: "unreadable" });
});

function body(chunks: string[]): ReadableStream<Uint8Array<ArrayBuffer>> {
  const encoder = new TextEncoder();
  return new ReadableStream({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      controller.close();
    },
  });
}

test("a stream is read event by event, however its bytes are split", async () => {
  const first = `data: ${JSON.stringify({ kind: "snapshot", snapshot })}\n\n`;
  const half = Math.floor(first.length / 2);
  const received: Received[] = [];
  const chunks = [
    ": keep-alive\n\n",
    first.slice(0, half),
    first.slice(half),
    'data: {"kind":"inv',
    'alidated"}\n\n: keep-alive\n\ndata: {"kind":"unavailable"}\n\n',
  ];
  for await (const message of streamMessages(body(chunks))) received.push(message);
  expect(received).toStrictEqual([
    { kind: "snapshot", snapshot },
    { kind: "unreadable" },
    { kind: "unavailable" },
  ]);
});

test("a stream that ends inside an event gives only the events it finished", async () => {
  const received: Received[] = [];
  const chunks = ['data: {"kind":"signed_out"}\n\n', 'data: {"kind":"unavail'];
  for await (const message of streamMessages(body(chunks))) received.push(message);
  expect(received).toStrictEqual([{ kind: "signed_out" }]);
});
