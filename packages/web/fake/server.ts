import { lstat, readFile } from "node:fs/promises";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { extname, join } from "node:path";
import { Readable } from "node:stream";
import { Type } from "typebox";
import { Check } from "typebox/value";
import {
  ControlBodySchema,
  SIGN_IN_PATH,
  STREAM_PATH,
  SignInBodySchema,
  ViewSchema,
  WINDOW_START,
  bearer,
  type View,
} from "../src/api/requests.ts";
import {
  AUDIENCE_ID_PATTERN,
  EVENT_ID_PATTERN,
  parsedJson,
  type ConversationKey,
  type FileLink,
  type StreamMessage,
} from "../src/api/stream.ts";
import { scenarios, type Scenario, type Stage } from "./scenarios.ts";
import {
  AWAY_ANSWER,
  EPOCH,
  PASSWORD,
  admit,
  link,
  snapshot,
  token,
  turnOf,
  type World,
} from "./world.ts";

type Clock = "manual" | "live";

export type Options = {
  port: number;
  /** A flat folder with the built page, served beside the API. */
  site: string | undefined;
  scenario: string;
  clock: Clock;
};

type Stream = { view: View; response: ServerResponse; last: string };

type Play = {
  name: string;
  scenario: Scenario;
  world: World;
  applied: number;
  clock: Clock;
  /** Milliseconds of this process's clock at which the scenario's own clock read zero. */
  began: number;
  sent: boolean;
  timer: NodeJS.Timeout | undefined;
};

/** The policy `serve.rs` gives every answer. */
const PAGE_POLICY =
  "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; " +
  "connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/** The files the provider serves from the page's folder. */
const SITE_TYPES: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".css": "text/css",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".woff2": "font/woff2",
};

const SelectionSchema = Type.Object({
  name: Type.String(),
  clock: Type.Optional(Type.Union([Type.Literal("manual"), Type.Literal("live")])),
});
const StepSchema = Type.Object({ expect: Type.Optional(Type.String()) });

function refuse(response: ServerResponse, status: number, error: string): void {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify({ error }));
}

function plain(response: ServerResponse, status: number, text: string): void {
  response.writeHead(status, { "content-type": "text/plain; charset=utf-8" });
  response.end(text);
}

/** As the provider's JSON routes require: JSON under another content type is a 415. */
function sentJson(request: IncomingMessage): boolean {
  return (
    request.headers["content-type"]?.split(";")[0]?.trim().toLowerCase() === "application/json"
  );
}

function json(response: ServerResponse, value: unknown): void {
  response.writeHead(200, { "content-type": "application/json" });
  response.end(JSON.stringify(value));
}

async function text(request: IncomingMessage, limit: number): Promise<string | undefined> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of request) {
    if (!Buffer.isBuffer(chunk)) continue;
    size += chunk.byteLength;
    if (size <= limit) chunks.push(chunk);
  }
  return size > limit ? undefined : Buffer.concat(chunks).toString("utf8");
}

/** A raster image's type by its bytes, as `files.rs` reads it. Anything else downloads. */
function typeOf(bytes: Uint8Array): FileLink["type"] {
  const start = Buffer.from(bytes.subarray(0, 12)).toString("latin1");
  if (start.startsWith("\u0089PNG\r\n\u001a\n")) return "image/png";
  if (start.startsWith("ÿØÿ")) return "image/jpeg";
  if (start.startsWith("GIF87a") || start.startsWith("GIF89a")) return "image/gif";
  if (start.startsWith("RIFF") && start.slice(8) === "WEBP") return "image/webp";
  return "application/octet-stream";
}

/** The view a stream's query asks for, by the rules of core's `ViewQuery`. */
export function viewOf(query: URLSearchParams): View | undefined {
  const known = new Set(["conversations", "agent_id", "audience_id", "turns"]);
  if ([...query.keys()].some((name) => !known.has(name))) return undefined;
  const window = (name: string): number | undefined => {
    const written = query.get(name);
    return written === null ? undefined : Number(written);
  };
  const agentId = query.get("agent_id");
  const audienceId = query.get("audience_id");
  const turns = window("turns");
  if (agentId === null && audienceId === null && turns !== undefined) return undefined;
  const view = {
    conversations: window("conversations") ?? WINDOW_START,
    open:
      agentId === null && audienceId === null
        ? null
        : { key: { agent_id: agentId, audience_id: audienceId }, turns: turns ?? WINDOW_START },
  };
  return Check(ViewSchema, view) ? view : undefined;
}

export type Fake = { port: number; close(): Promise<void> };

export async function serve(options: Options): Promise<Fake> {
  const streams = new Set<Stream>();
  /** Stream messages written and streams ended, since the process began. */
  const counts = { written: 0, ended: 0 };
  const requests: string[] = [];
  let release: (() => void) | undefined;
  let holding: Promise<void> | undefined;
  /** Callers of `/fake/state?wait=` that are answered once what they wait for holds. */
  let waiters: { ready(): boolean; resolve(): void }[] = [];
  let play = begin(options.scenario, options.clock);

  function notify(): void {
    for (const waiter of waiters.filter((each) => each.ready())) waiter.resolve();
    waiters = waiters.filter((each) => !each.ready());
  }

  async function until(ready: () => boolean): Promise<void> {
    if (!ready()) await new Promise<void>((resolve) => waiters.push({ ready, resolve }));
  }

  function begin(name: string, clock: Clock): Play {
    const scenario = scenarios[name];
    if (scenario === undefined) throw new Error(`the fake has no scenario ${name}`);
    const now = clock === "manual" ? EPOCH : Math.floor(Date.now() / 1000);
    return {
      name,
      scenario,
      world: scenario.world(now),
      applied: 0,
      clock,
      began: Date.now(),
      sent: false,
      timer: undefined,
    };
  }

  function write(stream: Stream, data: string): void {
    stream.response.write(`data: ${data}\n\n`);
    counts.written += 1;
  }

  /** Sends each stream the snapshot of its view, unless it is the one it has. */
  function publish(): void {
    if (!play.world.available) return;
    for (const stream of streams) {
      const message: StreamMessage = {
        kind: "snapshot",
        snapshot: snapshot(play.world, stream.view),
      };
      const data = JSON.stringify(message);
      if (data !== stream.last) {
        stream.last = data;
        write(stream, data);
      }
    }
  }

  function finish(last?: StreamMessage): void {
    for (const stream of streams) {
      if (last !== undefined) write(stream, JSON.stringify(last));
      stream.response.end();
      counts.ended += 1;
    }
    streams.clear();
    notify();
  }

  const stage: Stage = {
    get world() {
      return play.world;
    },
    away() {
      play.world.available = false;
      finish({ kind: "unavailable" });
    },
    returns() {
      play.world.available = true;
    },
    signOut() {
      play.world.password += 1;
      finish({ kind: "signed_out" });
    },
    endStreams() {
      finish();
    },
    mute() {
      streams.clear();
      notify();
    },
    say(data) {
      for (const stream of streams) write(stream, data);
    },
    release() {
      release?.();
    },
  };

  /** The scenario's next step, on the fake's clock at that step's time. */
  function apply(): string | undefined {
    const step = play.scenario.steps[play.applied];
    if (step === undefined) return undefined;
    play.applied += 1;
    play.world.now =
      play.clock === "manual" ? EPOCH + Math.floor(step.at / 1000) : Math.floor(Date.now() / 1000);
    step.run(stage);
    publish();
    return step.label;
  }

  /** On its own clock, the fake plays each step when its time has come. */
  function schedule(): void {
    clearTimeout(play.timer);
    const step = play.scenario.steps[play.applied];
    if (play.clock !== "live" || step === undefined) return;
    if (step.after === "send" && !play.sent) return;
    play.timer = setTimeout(
      () => {
        apply();
        schedule();
      },
      Math.max(play.began + step.at - Date.now(), 0),
    );
  }

  function select(name: string, clock: Clock): void {
    clearTimeout(play.timer);
    finish();
    release?.();
    play = begin(name, clock);
    requests.length = 0;
    schedule();
  }

  function state(): object {
    return {
      scenario: play.name,
      clock: play.clock,
      applied: play.applied,
      remaining: play.scenario.steps.slice(play.applied).map((step) => step.label),
      written: counts.written,
      ended: counts.ended,
      holding: holding !== undefined,
      streams: streams.size,
      requests,
    };
  }

  async function control(
    request: IncomingMessage,
    response: ServerResponse,
    url: URL,
  ): Promise<void> {
    if (request.method === "GET" && url.pathname === "/fake/state") {
      const wait = url.searchParams.get("wait");
      const count = Number(url.searchParams.get("count"));
      if (wait === "holding") await until(() => holding !== undefined);
      if (wait === "streams") await until(() => streams.size === count);
      json(response, state());
      return;
    }
    if (request.method === "GET" && url.pathname === "/fake/scenarios") {
      json(
        response,
        Object.entries(scenarios).map(([name, scenario]) => ({
          name,
          about: scenario.about,
          steps: scenario.steps.map((step) => step.label),
        })),
      );
      return;
    }
    const body = parsedJson((await text(request, 4096)) ?? "");
    if (request.method === "POST" && url.pathname === "/fake/scenario") {
      if (!Check(SelectionSchema, body) || scenarios[body.name] === undefined) {
        refuse(response, 404, "The fake has no such scenario. GET /fake/scenarios lists them.");
        return;
      }
      select(body.name, body.clock ?? "manual");
      json(response, state());
      return;
    }
    if (request.method === "POST" && url.pathname === "/fake/step") {
      const next = play.scenario.steps[play.applied]?.label;
      const expected = Check(StepSchema, body) ? body.expect : undefined;
      if (next === undefined || (expected !== undefined && expected !== next)) {
        refuse(
          response,
          409,
          `The next step of ${play.name} is ${next ?? "none: it has ended"}, not ${expected ?? "a step"}.`,
        );
        return;
      }
      apply();
      // For a measurement: when the step's snapshots had been written, on this machine's clock.
      const wrote_at = performance.timeOrigin + performance.now();
      json(response, { label: next, wrote_at, ...state() });
      return;
    }
    response.writeHead(404).end();
  }

  function stream(response: ServerResponse, url: URL): void {
    const view = viewOf(url.searchParams);
    if (view === undefined) {
      plain(response, 400, "a view is conversations, and agent_id with audience_id, and turns");
      return;
    }
    response.writeHead(200, { "content-type": "text/event-stream" });
    response.flushHeaders();
    const opened: Stream = { view, response, last: "" };
    if (!play.world.available) {
      write(opened, JSON.stringify({ kind: "unavailable" } satisfies StreamMessage));
      response.end();
      counts.ended += 1;
      return;
    }
    streams.add(opened);
    response.on("close", () => {
      streams.delete(opened);
      notify();
    });
    publish();
    notify();
  }

  async function message(
    request: IncomingMessage,
    response: ServerResponse,
    key: ConversationKey,
    eventId: string,
  ): Promise<void> {
    if (play.world.sending.kind === "hold_upload") {
      holding = new Promise<void>((resolve) => {
        release = resolve;
      });
      notify();
      await holding;
      holding = undefined;
      release = undefined;
    }
    // The body is read whole before any refusal: a browser answered while it uploads reports a broken connection.
    const upload = new Request("http://fake", {
      method: "PUT",
      headers: { "content-type": request.headers["content-type"] ?? "" },
      body: Readable.toWeb(request),
      duplex: "half",
    });
    // oxlint-disable-next-line typescript/no-deprecated -- a fake may buffer the forms of a page it trusts; a provider streams them
    const form = await upload.formData().catch(() => undefined);
    const held = play.world;
    const { limits, sending } = held;
    if (form === undefined) {
      refuse(response, 400, "A message is sent as multipart form data.");
      return;
    }
    const said = form.get("text");
    const files = form.getAll("file").filter((part) => part instanceof File);
    if ([...form.keys()].some((name) => name !== "text" && name !== "file")) {
      refuse(response, 400, "A message's parts are `text` and `file`.");
      return;
    }
    if (!held.agents.some((agent) => agent.id === key.agent_id)) {
      refuse(response, 422, `Carillon has no agent ${key.agent_id}.`);
      return;
    }
    const words = typeof said === "string" ? said : "";
    if (words.trim() === "" && files.length === 0) {
      refuse(response, 400, "A message needs text or a file.");
      return;
    }
    if (files.length > limits.files) {
      refuse(response, 413, `A message carries at most ${String(limits.files)} files.`);
      return;
    }
    const total = files.reduce((sum, file) => sum + file.size, 0);
    if (files.some((file) => file.size > limits.file_bytes) || total > limits.message_bytes) {
      const mib = (bytes: number) => String(Math.floor(bytes / (1024 * 1024)));
      refuse(
        response,
        413,
        `A file may have at most ${mib(limits.file_bytes)} MiB, and a message's files ${mib(limits.message_bytes)} MiB together.`,
      );
      return;
    }
    if (!held.available) {
      refuse(response, 503, AWAY_ANSWER);
      return;
    }
    if (sending.kind === "refuse") {
      refuse(response, sending.status, sending.error);
      return;
    }
    const links: FileLink[] = [];
    for (const file of files) {
      const bytes = new Uint8Array(await file.arrayBuffer());
      links.push(link(held, file.name === "" ? "file" : file.name, typeOf(bytes), bytes));
    }
    const receipt = admit(held, key, eventId, words, links);
    requests.push(`admitted ${eventId} as ${receipt.turn_id}${receipt.duplicate ? " again" : ""}`);
    if (sending.kind === "lose_receipt") {
      stage.away();
      refuse(response, 503, AWAY_ANSWER);
      return;
    }
    play.sent = true;
    publish();
    schedule();
    json(response, receipt);
  }

  async function press(
    request: IncomingMessage,
    response: ServerResponse,
    eventId: string,
  ): Promise<void> {
    const body = parsedJson((await text(request, 4096)) ?? "");
    if (!sentJson(request)) {
      plain(response, 415, "a control is JSON");
      return;
    }
    if (!Check(ControlBodySchema, body)) {
      plain(response, 422, "a control is an action: steer, interrupt or stop");
      return;
    }
    const pair = turnOf(play.world, eventId);
    if (!play.world.available) refuse(response, 503, AWAY_ANSWER);
    else if (pair === undefined) refuse(response, 404, "no such message");
    else if (pair.turn.progress.state === "ended")
      refuse(response, 409, "this turn is no longer running");
    else {
      play.world.pressed = { event_id: eventId, action: body.action };
      response.writeHead(204).end();
    }
  }

  async function api(request: IncomingMessage, response: ServerResponse, url: URL): Promise<void> {
    const path = url.pathname;
    if (path === SIGN_IN_PATH) {
      const body = await text(request, 4096);
      const guess = parsedJson(body ?? "");
      if (request.method !== "POST") response.writeHead(405).end();
      else if (body === undefined) response.writeHead(413).end();
      else if (!sentJson(request)) plain(response, 415, "a sign-in is JSON");
      else if (!Check(SignInBodySchema, guess)) plain(response, 422, "a sign-in is a password");
      else if (guess.password.trim() !== PASSWORD)
        refuse(response, 401, "That is not the web chat's password.");
      else json(response, { token: token(play.world) });
      return;
    }
    if (request.headers.authorization !== bearer(token(play.world))) {
      request.resume();
      refuse(response, 401, "This browser is not signed in. Sign in with the web chat's password.");
      return;
    }
    if (path === STREAM_PATH && request.method === "GET") {
      stream(response, url);
      return;
    }
    const conversation =
      /^\/api\/conversations\/(?<agent>[^/]+)\/(?<audience>[^/]+)\/(?:messages\/(?<event>[^/]+)|(?<commands>commands))$/u.exec(
        path,
      )?.groups;
    const pressed = /^\/api\/messages\/(?<event>[^/]+)\/control$/u.exec(path)?.groups;
    const eventId = conversation?.["event"] ?? pressed?.["event"];
    if (eventId !== undefined && !new RegExp(EVENT_ID_PATTERN, "u").test(eventId)) {
      request.resume();
      plain(response, 400, "a message is `m-` and a lowercase UUID");
      return;
    }
    if (conversation !== undefined) {
      const key = {
        agent_id: decodeURIComponent(conversation["agent"] ?? ""),
        audience_id: conversation["audience"] ?? "",
      };
      if (!new RegExp(AUDIENCE_ID_PATTERN, "u").test(key.audience_id)) {
        request.resume();
        plain(response, 400, "a conversation is a lowercase UUID");
      } else if (eventId !== undefined && request.method === "PUT") {
        await message(request, response, key, eventId);
      } else if (conversation["commands"] !== undefined && request.method === "GET") {
        if (!play.world.available) refuse(response, 503, AWAY_ANSWER);
        else if (!play.world.agents.some((agent) => agent.id === key.agent_id))
          refuse(response, 422, `Carillon has no agent ${key.agent_id}.`);
        else json(response, { commands: play.world.commands });
      } else response.writeHead(405).end();
      return;
    }
    if (pressed !== undefined && eventId !== undefined && request.method === "POST") {
      await press(request, response, eventId);
      return;
    }
    response.writeHead(404).end();
  }

  function file(response: ServerResponse, url: URL): void {
    const stored = play.world.files.get(decodeURIComponent(url.pathname.slice("/files/".length)));
    if (url.searchParams.get("sig") !== "fake") {
      refuse(
        response,
        403,
        "This file link is no longer good. Read the conversation again for a new one.",
      );
    } else if (stored === undefined) {
      refuse(response, 404, "Carillon does not have this file.");
    } else {
      const shown = stored.type === "application/octet-stream" ? "attachment" : "inline";
      response.writeHead(200, {
        "content-type": stored.type,
        "content-disposition": `${shown}; filename*=UTF-8''${encodeURIComponent(stored.name)}`,
        "content-length": stored.bytes.byteLength,
        "content-security-policy": "sandbox",
        "cache-control": "private, max-age=86400, immutable",
      });
      response.end(stored.bytes);
    }
  }

  /** A file of the page's flat folder, as `site.rs` serves it: a regular file of a type it knows. */
  async function site(response: ServerResponse, url: URL): Promise<void> {
    const chosen = url.searchParams.get("scenario");
    if (url.pathname === "/" && chosen !== null && scenarios[chosen] !== undefined)
      select(chosen, "live");
    const name = url.pathname === "/" ? "index.html" : url.pathname.slice(1);
    const type = SITE_TYPES[extname(name)];
    if (options.site === undefined || type === undefined || !/^[\w.-]+$/u.test(name)) {
      response.writeHead(404).end();
      return;
    }
    const path = join(options.site, name);
    const found = await lstat(path).catch(() => undefined);
    if (found?.isFile() !== true) {
      response.writeHead(404).end();
      return;
    }
    response.writeHead(200, { "content-type": type });
    response.end(await readFile(path));
  }

  async function handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const url = new URL(request.url ?? "/", "http://fake");
    response.setHeader("content-security-policy", PAGE_POLICY);
    response.setHeader("x-content-type-options", "nosniff");
    response.setHeader("referrer-policy", "no-referrer");
    response.setHeader("cache-control", "no-store");
    response.setHeader("cross-origin-resource-policy", "same-origin");
    if (url.pathname.startsWith("/fake/")) await control(request, response, url);
    else if (url.pathname.startsWith("/api/")) {
      await api(request, response, url);
      const where = url.pathname
        .replace(/m-[0-9a-f-]{36}/u, "<event>")
        .replace(/[0-9a-f-]{36}/u, "<audience>");
      requests.push(`${request.method ?? ""} ${where} ${String(response.statusCode)}`);
    } else if (url.pathname.startsWith("/files/")) file(response, url);
    else await site(response, url);
  }

  const server: Server = createServer((request, response) => {
    handle(request, response).catch((error: unknown) => {
      console.error(error);
      if (!response.headersSent) response.writeHead(500);
      response.end();
    });
  });
  const beat = setInterval(() => {
    if (play.clock === "live") for (const each of streams) each.response.write(": keep-alive\n\n");
  }, 10_000);
  await new Promise<void>((resolve) => server.listen(options.port, "127.0.0.1", resolve));
  schedule();
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("the fake has no port");
  return {
    port: address.port,
    async close() {
      clearInterval(beat);
      clearTimeout(play.timer);
      finish();
      release?.();
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}
