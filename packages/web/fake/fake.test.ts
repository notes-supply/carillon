/**
 * Plays every scenario against the fake over HTTP, as a page would, with
 * nothing but Node. Each scenario runs twice. A test fails when a stream
 * message is not one the contract reads, when an answer lacks the headers a
 * real page meets, or when the two runs differ by a byte.
 */
import { createHash } from "node:crypto";
import type { Static } from "typebox";
import { request as httpRequest } from "node:http";
import { afterAll, expect, test } from "vitest";
import {
  CommandCatalogSchema,
  SIGN_IN_PATH,
  SentSchema,
  SignedInSchema,
  bearer,
  commandsPath,
  controlBody,
  controlPath,
  messageForm,
  messagePath,
  readAnswer,
  signInBody,
  streamPath,
  type View,
} from "../src/api/requests.ts";
import {
  streamMessages,
  type ChatSnapshot,
  type Control,
  type ConversationKey,
  type MessageSource,
  type Received,
} from "../src/api/stream.ts";
import { scenarios } from "./scenarios.ts";
import { serve, viewOf } from "./server.ts";
import { StateSchema, SteppedSchema } from "./start.ts";
import { PASSWORD, audience, key } from "./world.ts";

const fake = await serve({ port: 0, site: undefined, scenario: "empty", clock: "manual" });
afterAll(() => fake.close());
const origin = `http://127.0.0.1:${String(fake.port)}`;

type State = Static<typeof StateSchema>;

/** What the driver has read of every stream since the process began. */
const read = { messages: 0, ends: 0 };
let arrived: (() => void) | undefined;

type Run = {
  lines: string[];
  token: string;
  stream: AbortController | undefined;
  last: ChatSnapshot | undefined;
  unreadable: number;
  faults: string[];
};

function digest(text: string): string {
  return createHash("sha256").update(text).digest("hex");
}

async function state(query = ""): Promise<State> {
  const response = await fetch(`${origin}/fake/state${query}`);
  const answered = readAnswer(StateSchema, await response.text());
  if (answered === undefined) throw new Error("the fake did not say how it stands");
  return answered;
}

/** Waits until the driver has read everything the fake wrote and seen every stream it ended. */
async function settle(): Promise<void> {
  const { written, ended } = await state();
  while (read.messages < written || read.ends < ended)
    await new Promise<void>((resolve) => {
      arrived = resolve;
    });
}

function headersOf(run: Run, response: Response, what: string): void {
  const policy = response.headers.get("content-security-policy") ?? "";
  if (!policy.includes("script-src 'self'") && policy !== "sandbox")
    run.faults.push(`${what} has no content security policy`);
  if (response.headers.get("x-content-type-options") !== "nosniff")
    run.faults.push(`${what} may be sniffed`);
  if (response.headers.get("cache-control") === null) run.faults.push(`${what} may be cached`);
}

async function ask(
  run: Run,
  method: string,
  path: string,
  body?: string | FormData,
): Promise<string> {
  const response = await fetch(origin + path, {
    method,
    headers: { authorization: bearer(run.token) },
    ...(body === undefined ? {} : { body }),
  });
  const answer = await response.text();
  headersOf(run, response, `${method} ${path}`);
  run.lines.push(`> ${method} ${path} ${String(response.status)} ${answer}`);
  await settle();
  return answer;
}

async function signIn(run: Run, password: string): Promise<void> {
  const answer = await ask(run, "POST", SIGN_IN_PATH, signInBody(password));
  run.token = readAnswer(SignedInSchema, answer)?.token ?? run.token;
}

function describe(message: Received): string {
  if (message.kind !== "snapshot") return message.kind;
  const { conversations, more, open } = message.snapshot;
  const listed = `${String(conversations.length)} conversations${more ? " and more" : ""}`;
  if (open === null) return `snapshot of ${listed}, none open`;
  if (open.kind === "unrecorded") return `snapshot of ${listed}, open one unrecorded`;
  const states = open.turns.map(({ progress }) =>
    progress.state === "ended" ? progress.ending.kind : progress.state,
  );
  const tally = [...new Set(states)].map(
    (each) => `${String(states.filter((other) => other === each).length)} ${each}`,
  );
  return `snapshot of ${listed}, open one has ${String(open.turns.length)} turns${open.earlier ? " and earlier ones" : ""} (${tally.join(", ")})`;
}

/** Opens a stream of `view` in place of the one before it, and reads it until it ends. */
async function watch(run: Run, view: View): Promise<void> {
  expect(viewOf(new URL(origin + streamPath(view)).searchParams)).toStrictEqual(view);
  run.stream?.abort();
  await state("?wait=streams&count=0");
  const stream = new AbortController();
  run.stream = stream;
  const response = await fetch(origin + streamPath(view), {
    headers: { authorization: bearer(run.token) },
    signal: stream.signal,
  });
  headersOf(run, response, "the stream");
  run.lines.push(`> GET ${streamPath(view)} ${String(response.status)}`);
  if (response.status !== 200 || response.body === null) {
    run.lines.push(`  ${await response.text()}`);
    return;
  }
  void (async () => {
    for await (const message of streamMessages(response.body ?? new ReadableStream())) {
      if (message.kind === "snapshot") run.last = message.snapshot;
      if (message.kind === "unreadable") run.unreadable += 1;
      run.lines.push(`< ${describe(message)} ${digest(JSON.stringify(message))}`);
      read.messages += 1;
      arrived?.();
    }
    run.lines.push("< the stream ended");
    read.ends += 1;
    arrived?.();
  })().catch((error: unknown) => {
    if (!stream.signal.aborted) throw error;
  });
  await settle();
}

async function step(run: Run): Promise<boolean> {
  const response = await fetch(`${origin}/fake/step`, { method: "POST", body: "{}" });
  const stepped = readAnswer(SteppedSchema, await response.text());
  if (stepped === undefined) return false;
  run.lines.push(`# ${stepped.label}`);
  await settle();
  return true;
}

async function rest(run: Run): Promise<void> {
  while (await step(run));
}

const DEFAULT: View = { conversations: 50, open: null };

function opening(conversation: ConversationKey, turns = 50): View {
  return { conversations: 50, open: { key: conversation, turns } };
}

function eventId(n: number): string {
  return `m-${audience(900_000 + n)}`;
}

async function send(run: Run, conversation: ConversationKey, n: number, text: string) {
  const answer = await ask(
    run,
    "PUT",
    messagePath(conversation, eventId(n)),
    messageForm(text, []),
  );
  return readAnswer(SentSchema, answer);
}

function messageOf(run: Run, text: string): MessageSource {
  const open = run.last?.open;
  const turns = open?.kind === "recorded" ? open.turns : [];
  const source = turns
    .map((turn) => turn.source)
    .find((each) => each.kind === "message" && each.text === text);
  if (source?.kind !== "message")
    throw new Error(`the open conversation lacks the message ${text}`);
  return source;
}

async function press(run: Run, message: MessageSource, action: Control): Promise<void> {
  await ask(run, "POST", controlPath(message), controlBody(action));
}

/** Signs in, wrongly and then rightly, and opens the first conversation listed. */
async function arrive(run: Run): Promise<void> {
  await signIn(run, "a wrong password");
  await watch(run, DEFAULT);
  await signIn(run, PASSWORD);
  await watch(run, DEFAULT);
  const first = run.last?.conversations[0];
  if (first !== undefined) await watch(run, opening(first.key));
}

/** Sends 64 MiB that the provider does not read until the scenario's step. */
async function upload(run: Run): Promise<void> {
  const file = new File([new Uint8Array(64 * 1024 * 1024)], "large.bin");
  const form = new Response(messageForm("A large file.", [file]));
  const body = Buffer.from(await form.arrayBuffer());
  const path = messagePath(key(1), eventId(1));
  let answer: string | undefined;
  const answered = new Promise<void>((resolve, reject) => {
    const sending = httpRequest(
      origin + path,
      {
        method: "PUT",
        headers: {
          authorization: bearer(run.token),
          "content-type": form.headers.get("content-type") ?? "",
          "content-length": body.byteLength,
        },
      },
      (response) => {
        const chunks: Buffer[] = [];
        response.on("data", (chunk: Buffer) => chunks.push(chunk));
        response.on("end", () => {
          answer = `${String(response.statusCode)} ${Buffer.concat(chunks).toString("utf8")}`;
          resolve();
        });
      },
    );
    sending.on("error", reject);
    sending.end(body);
  });
  await state("?wait=holding");
  expect(answer, "the provider answers nothing while it holds the upload").toBe(undefined);
  run.lines.push(
    `> PUT ${path} of ${String(body.byteLength)} bytes is held by the provider, unanswered`,
  );
  await step(run);
  await answered;
  run.lines.push(`> PUT ${path} ${answer ?? ""}`);
  await settle();
}

const scripts: Record<string, (run: Run) => Promise<void>> = {
  async turn(run) {
    await arrive(run);
    await send(run, key(1), 1, "Write the release notes.\nKeep them short.");
    await rest(run);
    const open = run.last?.open;
    const newest = open?.kind === "recorded" ? open.turns.at(-1)?.progress : undefined;
    const files =
      newest?.state === "ended" && newest.ending.kind === "answered" ? newest.ending.files : [];
    expect(files.length, "the answer has its two files").toBe(2);
    for (const file of files) {
      const response = await fetch(origin + file.url);
      const bytes = (await response.arrayBuffer()).byteLength;
      expect(bytes).toBe(file.size);
      run.lines.push(
        `> GET ${file.url} ${String(response.status)} ${response.headers.get("content-type") ?? ""}, ${response.headers.get("content-disposition") ?? ""}, ${String(bytes)} bytes`,
      );
    }
  },
  async "first-message"(run) {
    await arrive(run);
    await watch(run, opening(key(7)));
    await send(run, key(7), 1, "\n  Name the new release.  \nSomething short.");
    await rest(run);
  },
  async "send-refused"(run) {
    await arrive(run);
    await send(run, key(1), 1, "Write the release notes.");
    await rest(run);
    await send(run, key(1), 1, "Write the release notes.");
  },
  async "send-lost-receipt"(run) {
    await arrive(run);
    expect(await send(run, key(1), 1, "Write the release notes.")).toBe(undefined);
    await watch(run, opening(key(1)));
    await send(run, key(1), 1, "Write the release notes.");
    await rest(run);
    await watch(run, opening(key(1)));
    const receipt = await send(run, key(1), 1, "Write the release notes.");
    expect(receipt?.duplicate, "the same event ID is answered as a duplicate").toBe(true);
  },
  async "send-held"(run) {
    await arrive(run);
    await upload(run);
  },
  async commands(run) {
    await arrive(run);
    const answer = await ask(run, "GET", commandsPath(key(1)));
    expect(readAnswer(CommandCatalogSchema, answer)?.commands.length).toBe(3);
    await ask(run, "GET", commandsPath({ agent_id: "nobody", audience_id: audience(1) }));
    await send(run, key(1), 1, "/status now");
    await send(run, key(1), 2, "/statusquo is not a command");
    await rest(run);
    expect(messageOf(run, "/status now").command).toBe(true);
    expect(messageOf(run, "/statusquo is not a command").command).toBe(false);
  },
  async unavailable(run) {
    await arrive(run);
    await step(run);
    await watch(run, opening(key(1)));
    await ask(run, "GET", commandsPath(key(1)));
    await step(run);
    await watch(run, opening(key(1)));
  },
  async "stream-ends"(run) {
    await arrive(run);
    await step(run);
    await watch(run, opening(key(1)));
    await step(run);
  },
  async "signed-out"(run) {
    await arrive(run);
    await step(run);
    await watch(run, opening(key(1)));
    await signIn(run, PASSWORD);
    await watch(run, opening(key(1)));
  },
  async earlier(run) {
    await arrive(run);
    await watch(run, opening(key(1), 100));
    await watch(run, opening(key(1), 150));
    await ask(run, "GET", "/api/stream?conversations=0");
    await ask(run, "GET", "/api/stream?turns=50");
    await ask(
      run,
      "GET",
      `/api/stream?agent_id=assistant&audience_id=${audience(1)}&before=t-0001`,
    );
  },
  async large(run) {
    await arrive(run);
    await watch(run, opening(key(1), 500));
    await rest(run);
  },
};

for (const [name, action, text] of [
  ["controls: steer the waiting message", "steer", "Use last week's figures instead."],
  ["controls: interrupt for the waiting message", "interrupt", "Use last week's figures instead."],
  ["controls: stop the waiting message", "stop", "Use last week's figures instead."],
  ["controls: stop the running answer", "stop", "Write the long report."],
] as const)
  scripts[name] = async (run) => {
    await arrive(run);
    const answering = messageOf(run, "Write the long report.");
    await press(run, messageOf(run, text), action);
    await rest(run);
    await press(run, answering, "stop");
    await press(run, { ...answering, event_id: eventId(404) }, "stop");
  };

async function play(name: string): Promise<Run> {
  const scenario = name.split(":")[0] ?? name;
  const selected = await fetch(`${origin}/fake/scenario`, {
    method: "POST",
    body: JSON.stringify({ name: scenario, clock: "manual" }),
  });
  expect(selected.status, `the fake has the scenario ${scenario}`).toBe(200);
  const run: Run = {
    lines: [],
    token: "",
    stream: undefined,
    last: undefined,
    unreadable: 0,
    faults: [],
  };
  await settle();
  const script = scripts[name];
  if (script === undefined) {
    await arrive(run);
    await rest(run);
  } else await script(run);
  expect((await state()).remaining, `${name} played every step`).toStrictEqual([]);
  run.stream?.abort();
  await state("?wait=streams&count=0");
  return run;
}

const names = [
  ...Object.keys(scenarios).filter((name) => name !== "controls"),
  ...Object.keys(scripts).filter((name) => name.startsWith("controls:")),
];

// The two large scenarios each send 240 snapshots of 500 turns, twice.
const BOUND = { timeout: 60_000 };

test.each(names)("%s plays the same twice, in messages the contract reads", BOUND, async (name) => {
  const first = await play(name);
  const second = await play(name);
  expect(second.lines).toStrictEqual(first.lines);
  expect(first.unreadable).toBe(name === "unreadable" ? 1 : 0);
  expect(first.faults).toStrictEqual([]);
});
