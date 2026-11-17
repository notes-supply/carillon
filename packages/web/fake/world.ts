import type { Command, View } from "../src/api/requests.ts";
import type {
  Activity,
  Agent,
  ChatSnapshot,
  Control,
  Conversation,
  ConversationKey,
  Ending,
  FileLink,
  Limits,
  Turn,
} from "../src/api/stream.ts";

/** 2026-11-15T12:00:00Z: when every scenario begins, on the fake's own clock. */
export const EPOCH = Date.UTC(2026, 10, 15, 12, 0, 0) / 1000;
export const PASSWORD = "correct horse battery staple";
const AGENT = "assistant";
export const AWAY_ANSWER = "Carillon is not answering right now. Try again in a moment.";

type Sending =
  | { kind: "accept" }
  | { kind: "refuse"; status: number; error: string }
  | { kind: "lose_receipt" }
  | { kind: "hold_upload" }
  | { kind: "answer_before_snapshot" };

type StoredFile = { name: string; type: FileLink["type"]; bytes: Uint8Array };

export type Recorded = {
  key: ConversationKey;
  title: string;
  started_at: number;
  active_at: number;
  activity: number;
  turns: Turn[];
};

export type World = {
  unixSeconds: number;
  marker: number;
  turns: number;
  passwordVersion: number;
  available: boolean;
  window: "kept" | "ignored";
  sending: Sending;
  agents: Agent[];
  limits: Limits;
  custom_site: boolean;
  conversations: Recorded[];
  commands: Command[];
  turnByEvent: Map<string, string>;
  files: Map<string, StoredFile>;
  pressed: { event_id: string; action: Control } | undefined;
};

export function world(unixSeconds: number): World {
  return {
    unixSeconds,
    marker: 0,
    turns: 0,
    passwordVersion: 1,
    available: true,
    window: "kept",
    sending: { kind: "accept" },
    agents: [{ id: AGENT, name: "Carillon" }],
    limits: { files: 10, file_bytes: 1024 * 1024 * 1024, message_bytes: 2 * 1024 * 1024 * 1024 },
    custom_site: false,
    conversations: [],
    commands: [],
    turnByEvent: new Map(),
    files: new Map(),
    pressed: undefined,
  };
}

export function token(held: World): string {
  return `fake-token-${String(held.passwordVersion)}`;
}

export function audience(n: number): string {
  return `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;
}

export function key(n: number): ConversationKey {
  return { agent_id: AGENT, audience_id: audience(n) };
}

function same(one: ConversationKey, other: ConversationKey): boolean {
  return one.agent_id === other.agent_id && one.audience_id === other.audience_id;
}

export function find(held: World, wanted: ConversationKey): Recorded | undefined {
  return held.conversations.find((conversation) => same(conversation.key, wanted));
}

function unfinished(turn: Turn): boolean {
  return turn.progress.state !== "ended";
}

function summary(conversation: Recorded): Conversation {
  return {
    key: conversation.key,
    title: conversation.title,
    started_at: conversation.started_at,
    active_at: conversation.active_at,
    activity: conversation.activity,
    at_work: conversation.turns.filter(unfinished).length,
  };
}

export function snapshot(held: World, view: View): ChatSnapshot {
  const latest = held.conversations.toSorted((one, other) => other.activity - one.activity);
  const open = view.open;
  const recorded = open === null ? undefined : find(held, open.key);
  const shown = held.window === "ignored" ? Infinity : (open?.turns ?? 0);
  return {
    agents: held.agents,
    limits: held.limits,
    custom_site: held.custom_site,
    conversations: latest.slice(0, view.conversations).map(summary),
    more: latest.length > view.conversations,
    open:
      open === null
        ? null
        : recorded === undefined
          ? { kind: "unrecorded", key: open.key }
          : {
              kind: "recorded",
              conversation: summary(recorded),
              turns: recorded.turns.slice(-shown),
              earlier: recorded.turns.length > shown,
            },
  };
}

function touch(held: World, conversation: Recorded): void {
  held.marker += 1;
  conversation.activity = held.marker;
  conversation.active_at = held.unixSeconds;
}

function nextTurn(held: World): string {
  held.turns += 1;
  return `t-${String(held.turns).padStart(4, "0")}`;
}

export function link(
  held: World,
  name: string,
  type: FileLink["type"],
  bytes: Uint8Array,
): FileLink {
  const path = `f/${String(held.files.size + 1)}`;
  held.files.set(path, { name, type, bytes });
  const expires = (Math.floor(held.unixSeconds / 86_400) + 2) * 86_400;
  return {
    name,
    size: bytes.byteLength,
    type,
    url: `/files/${path}?name=${encodeURIComponent(name)}&exp=${String(expires)}&sig=fake`,
  };
}

function titleOf(text: string): string {
  const line = text.split(/\r?\n/u).find((candidate) => candidate.trim() !== "") ?? "";
  return Array.from(line.trim()).slice(0, 80).join("");
}

function invokes(held: World, text: string): boolean {
  const [first = ""] = text.split(/\s/u, 1);
  return held.commands.some((command) => first === `/${command.name}`);
}

export function admit(
  held: World,
  wanted: ConversationKey,
  eventId: string,
  text: string,
  files: FileLink[],
): { turn_id: string; duplicate: boolean } {
  const known = held.turnByEvent.get(eventId);
  if (known !== undefined) return { turn_id: known, duplicate: true };
  let conversation = find(held, wanted);
  if (conversation === undefined) {
    conversation = {
      key: wanted,
      title: titleOf(text),
      started_at: held.unixSeconds,
      active_at: held.unixSeconds,
      activity: 0,
      turns: [],
    };
    held.conversations.push(conversation);
  }
  const turn: Turn = {
    turn_id: nextTurn(held),
    source: { kind: "message", event_id: eventId, text, command: invokes(held, text), files },
    accepted_at: held.unixSeconds,
    progress: { state: "queued" },
  };
  conversation.turns.push(turn);
  held.turnByEvent.set(eventId, turn.turn_id);
  touch(held, conversation);
  return { turn_id: turn.turn_id, duplicate: false };
}

export function open(held: World, conversation: Recorded, label: string | undefined): Turn {
  const turn: Turn = {
    turn_id: nextTurn(held),
    source: label === undefined ? { kind: "opened" } : { kind: "opened", label },
    accepted_at: held.unixSeconds,
    progress: { state: "queued" },
  };
  conversation.turns.push(turn);
  touch(held, conversation);
  return turn;
}

export function run(turn: Turn, activity: Activity | null, controls: Control[]): void {
  turn.progress = { state: "running", activity, controls };
}

export function end(
  held: World,
  conversation: Recorded,
  turn: Turn,
  ending: Ending,
  report?: Activity,
): void {
  turn.progress =
    report === undefined
      ? { state: "ended", finished_at: held.unixSeconds, ending }
      : { state: "ended", finished_at: held.unixSeconds, ending, report };
  touch(held, conversation);
}

export function working(held: World): { conversation: Recorded; turn: Turn } {
  const found = held.conversations
    .flatMap((conversation) => conversation.turns.map((turn) => ({ conversation, turn })))
    .filter(({ turn }) => unfinished(turn))
    .toSorted((one, other) => other.turn.turn_id.localeCompare(one.turn.turn_id))[0];
  if (found === undefined) throw new Error("the scenario has no turn at work for this step");
  return found;
}

export function turnOf(
  held: World,
  eventId: string,
): { conversation: Recorded; turn: Turn } | undefined {
  for (const conversation of held.conversations)
    for (const turn of conversation.turns)
      if (turn.source.kind === "message" && turn.source.event_id === eventId)
        return { conversation, turn };
  return undefined;
}
