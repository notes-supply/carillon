/**
 * Attachments through real Pi sessions, Pi's own image normalization and
 * provider conversion, and real tools. Only inference is a local fixture:
 * it records what Pi sends and answers with scripted replies.
 */
import { createHash, randomBytes } from "node:crypto";
import { Type, type Static } from "typebox";
import { Parse } from "typebox/value";
import { once } from "node:events";
import { mkdir, mkdtemp, readFile, readdir, rename, rm, writeFile } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { deflateSync } from "node:zlib";
import { afterEach, expect, it } from "vitest";
import type { TurnAttachment } from "@carillon/protocol";
import { createPiRuntime, type RuntimeOperations } from "@carillon/runtime-pi";
import { conversationDirectory } from "./namespace.ts";
import { DEFAULT_INLINE_IMAGES, prepareAttachments } from "./attachments.ts";

/** The parts of an OpenAI-compatible request these tests read. */
const PartSchema = Type.Object({
  type: Type.String(),
  text: Type.Optional(Type.String()),
  image_url: Type.Optional(Type.Object({ url: Type.String() })),
});
const SentSchema = Type.Object({
  messages: Type.Array(
    Type.Object({
      role: Type.String(),
      content: Type.Union([Type.String(), Type.Array(PartSchema), Type.Null()]),
    }),
  ),
});
type Sent = Static<typeof SentSchema>;
type Content = Sent["messages"][number]["content"] | undefined;

type Reply = string | ((sent: Sent) => { tool: string; arguments: unknown } | string);

const cleanup: (() => Promise<void>)[] = [];
afterEach(async () => {
  for (const step of cleanup.splice(0).reverse()) await step();
});

/** A local OpenAI-compatible endpoint answering with `replies` in order. */
async function inference(replies: Reply[]) {
  const sent: Sent[] = [];
  const server: Server = createServer((request, response) => {
    let raw = "";
    request.on("data", (part: Buffer) => {
      raw += part.toString();
    });
    request.on("end", () => {
      const body = Parse(SentSchema, JSON.parse(raw));
      sent.push(body);
      const reply = replies.shift() ?? "done";
      const answer = typeof reply === "string" ? reply : reply(body);
      const delta =
        typeof answer === "string"
          ? { role: "assistant", content: answer }
          : {
              role: "assistant",
              tool_calls: [
                {
                  index: 0,
                  id: `call-${String(sent.length)}`,
                  type: "function",
                  function: { name: answer.tool, arguments: JSON.stringify(answer.arguments) },
                },
              ],
            };
      const chunk = (value: unknown, finish: string | null) =>
        `data: ${JSON.stringify({ id: "fixture", object: "chat.completion.chunk", created: 1, model: "vision", choices: [{ index: 0, delta: value, finish_reason: finish }] })}\n\n`;
      response.writeHead(200, { "content-type": "text/event-stream" });
      response.end(
        `${chunk(delta, null)}${chunk({}, typeof answer === "string" ? "stop" : "tool_calls")}data: [DONE]\n\n`,
      );
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  cleanup.push(async () => {
    server.closeAllConnections();
    await new Promise<void>((done) => server.close(() => done()));
  });
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("fixture has no port");
  return { sent, endpoint: `http://127.0.0.1:${String(address.port)}/v1` };
}

async function setup(
  input: ("text" | "image")[],
  replies: Reply[],
  policy = DEFAULT_INLINE_IMAGES,
  others: { id: string; input: ("text" | "image")[] }[] = [],
) {
  const root = await mkdtemp(join(tmpdir(), "carillon-attachments-"));
  cleanup.push(() => rm(root, { recursive: true, force: true }));
  const state = join(root, "pi");
  const resources = join(root, "resources");
  const workspace = join(root, "workspace");
  for (const directory of [state, resources, workspace]) await mkdir(directory);
  const fixture = await inference(replies);
  await writeFile(
    join(state, "settings.json"),
    JSON.stringify({ defaultProvider: "fixture", defaultModel: "vision" }),
  );
  await writeFile(
    join(state, "models.json"),
    JSON.stringify({
      providers: {
        fixture: {
          baseUrl: fixture.endpoint,
          api: "openai-completions",
          apiKey: "test-only",
          models: [{ id: "vision", input }, ...others],
        },
      },
    }),
  );
  const runtime = await createPiRuntime({
    workspace,
    stateDirectory: state,
    inlineImages: policy,
  });
  cleanup.push(() => runtime.close());
  return { root, state, resources, workspace, runtime, sent: fixture.sent };
}

/** A real, deterministic PNG of the given size; noisy pixels resist compression. */
function png(width: number, height: number, noisy = false): Buffer {
  const crcTable = Array.from({ length: 256 }, (_, n) => {
    let c = n;
    for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c >>> 0;
  });
  const crc = (bytes: Buffer) => {
    let c = 0xffffffff;
    for (const byte of bytes) c = (crcTable[(c ^ byte) & 0xff] ?? 0) ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  };
  const chunk = (type: string, data: Buffer) => {
    const length = Buffer.alloc(4);
    length.writeUInt32BE(data.length);
    const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
    const sum = Buffer.alloc(4);
    sum.writeUInt32BE(crc(body));
    return Buffer.concat([length, body, sum]);
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  header.set([8, 2, 0, 0, 0], 8);
  const row = (y: number) =>
    Buffer.concat([
      Buffer.from([0]),
      noisy ? randomBytes(width * 3) : Buffer.alloc(width * 3, (y * 7) % 256),
    ]);
  const pixels = Buffer.concat(Array.from({ length: height }, (_, y) => row(y)));
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(pixels)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

/** Files as core captures them: one capture directory, named by ordinal. */
async function capture(
  root: string,
  files: { name: string; bytes: Buffer; type?: string }[],
): Promise<TurnAttachment[]> {
  const capture = "0190a6e0-0000-7000-8000-0000000000a1";
  await mkdir(join(root, capture), { recursive: true });
  return await Promise.all(
    files.map(async (file, ordinal) => {
      await writeFile(join(root, capture, String(ordinal)), file.bytes);
      return {
        path: `${capture}/${String(ordinal)}`,
        name: file.name,
        size: file.bytes.length,
        ...(file.type === undefined ? {} : { content_type: file.type }),
        sha256: createHash("sha256").update(file.bytes).digest("hex"),
      };
    }),
  );
}

function turn(
  resources: string,
  attachmentRoot: string,
  input: string,
  attachments: TurnAttachment[] = [],
  correlation = input,
) {
  return {
    correlation_id: correlation,
    interface_id: "discord-private",
    agent_id: "default",
    audience_id: "files",
    input,
    resource_root: resources,
    attachment_root: attachmentRoot,
    attachments,
  };
}

const text = (content: Content): string => {
  if (content === undefined || content === null) throw new Error("message has no content");
  return typeof content === "string"
    ? content
    : content
        .filter((part) => part.type === "text")
        .map((part) => part.text ?? "")
        .join("\n");
};

const images = (content: Content): string[] =>
  typeof content === "string" || content === undefined || content === null
    ? []
    : content.filter((part) => part.type === "image_url").map((part) => part.image_url?.url ?? "");

function users(sent: Sent) {
  return sent.messages.filter((message) => message.role === "user");
}

/** The person's message: the last user message that is not a note. */
function prompt(sent: Sent | undefined) {
  return users(sent ?? { messages: [] })
    .filter((message) => !text(message.content).startsWith("[Carillon attachments]"))
    .at(-1);
}

/** The attachment notes the request carries, in order. */
function notes(sent: Sent | undefined): string[] {
  return users(sent ?? { messages: [] })
    .map((message) => text(message.content))
    .filter((content) => content.startsWith("[Carillon attachments]"));
}

function system(sent: Sent): string {
  return sent.messages
    .filter((message) => message.role === "system" || message.role === "developer")
    .map((message) => text(message.content))
    .join("\n");
}

async function execute(runtime: RuntimeOperations, request: ReturnType<typeof turn>) {
  return (await runtime.executeTurn(request)).output;
}

it("passes suitable images natively, lists every file and escapes hostile names", async () => {
  const { root, resources, runtime, sent } = await setup(["text", "image"], ["seen"], {
    ...DEFAULT_INLINE_IMAGES,
    max_megapixels: 1,
  });
  const store = join(root, "attachments");
  const first = png(64, 48);
  const second = png(32, 32);
  const attachments = await capture(store, [
    { name: "first.png", bytes: first, type: "image/png" },
    { name: "report.pdf", bytes: Buffer.from("%PDF-1.7 not an image"), type: "application/pdf" },
    { name: "evil\nname‮.png", bytes: png(1200, 1000), type: "image/png" },
    { name: "second.png", bytes: second },
  ]);
  expect(await execute(runtime, turn(resources, store, "Compare these.", attachments))).toBe(
    "seen",
  );
  const [request] = sent;
  if (request === undefined) throw new Error("Pi made no model request");
  const message = prompt(request);
  const [listed = ""] = notes(request);
  // Pi's own normalization passed the small images through, in order.
  expect(text(message?.content)).toBe("Compare these.");
  expect(images(message?.content)).toEqual([
    `data:image/png;base64,${first.toString("base64")}`,
    `data:image/png;base64,${second.toString("base64")}`,
  ]);
  expect(listed).toContain("[Carillon attachments]");
  expect(listed).toContain(`1. path: ${attachments[0]?.path ?? ""}; name: "first.png"`);
  expect(listed.match(/sent inline as an image/gu)).toHaveLength(2);
  expect(listed).toMatch(
    /2\. path: \S+; name: "report.pdf"; size: \S+ bytes; declared type: "application\/pdf"; file$/mu,
  );
  expect(listed).toContain('name: "evil\\nname\\u{202e}.png"');
  expect(listed).toContain(
    "not passed as an inline image: 1.2 megapixels exceeds the 1 megapixel limit",
  );
  expect(listed).not.toContain("‮");
  expect(listed).not.toContain(store);
  const instructions = system(request);
  expect(instructions).toContain("<carillon_attachments>");
  expect(instructions).toContain(`kept in ${store}.`);
});

it("keeps earlier files usable after reopening and relocating the installation", async () => {
  const { root, resources, runtime, sent, state } = await setup(
    ["text", "image"],
    [
      "noted",
      "still here",
      (request) => {
        // The model resolves the note's relative path against the current root.
        const path = /path: (\S+);/u.exec(
          users(request)
            .map((message) => text(message.content))
            .join("\n"),
        )?.[1];
        const directory = /kept in (\S+)\. /u.exec(system(request))?.[1];
        return { tool: "read", arguments: { path: `${directory ?? ""}/${path ?? ""}` } };
      },
      "read it",
    ],
  );
  const store = join(root, "attachments");
  const attachments = await capture(store, [
    { name: "plan.txt", bytes: Buffer.from("the original plan\n"), type: "text/plain" },
  ]);
  await execute(runtime, turn(resources, store, "Keep this file.", attachments, "one"));
  await execute(runtime, turn(resources, store, "Anything else?", [], "two"));
  expect(system(sent[1] ?? { messages: [] })).toContain(`kept in ${store}.`);

  const moved = join(root, "relocated");
  await rename(store, moved);
  expect(await execute(runtime, turn(resources, moved, "Read the plan again.", [], "three"))).toBe(
    "read it",
  );
  const last = sent.at(-1);
  expect(system(last ?? { messages: [] })).toContain(`kept in ${moved}.`);
  const result = last?.messages.find((message) => message.role === "tool");
  expect(text(result?.content)).toContain("the original plan");

  // Pi's native history keeps the note as written: relative, never rewritten.
  const directory = conversationDirectory(join(state, "sessions"), turn(resources, store, ""));
  const [file] = await readdir(directory);
  const history = await readFile(join(directory, file ?? ""), "utf8");
  expect(history).toContain('"customType":"carillon-attachments"');
  expect(history).toContain(attachments[0]?.path ?? "missing");
  expect(
    history.split("\n").filter((line) => line.includes("[Carillon attachments]")),
  ).toHaveLength(1);
  expect(history.split("\n").find((line) => line.includes("[Carillon attachments]"))).not.toContain(
    store,
  );
});

it("gives text-only models Pi's native image placeholder", async () => {
  const { root, resources, runtime, sent } = await setup(["text"], ["no eyes"]);
  const store = join(root, "attachments");
  const attachments = await capture(store, [{ name: "photo.png", bytes: png(16, 16) }]);
  expect(await execute(runtime, turn(resources, store, "What is this?", attachments))).toBe(
    "no eyes",
  );
  const message = prompt(sent[0]);
  expect(images(message?.content)).toEqual([]);
  expect(text(message?.content)).toContain("image omitted: model does not support images");
});

it("leaves a corrupt image to Pi's omission hint and keeps it a file", async () => {
  const { root, resources, runtime, sent } = await setup(["text", "image"], ["checked"]);
  const store = join(root, "attachments");
  const corrupt = png(40, 40);
  corrupt.fill(0, 41, corrupt.length - 12);
  const attachments = await capture(store, [{ name: "broken.png", bytes: corrupt }]);
  await execute(runtime, turn(resources, store, "Look.", attachments));
  const message = prompt(sent[0]);
  expect(images(message?.content)).toEqual([]);
  expect(text(message?.content)).toContain("[Image omitted");
  expect(notes(sent[0]).join("\n")).toContain('name: "broken.png"');
});

it("keeps slash commands and input handlers native when files are attached", async () => {
  const { root, resources, runtime, sent, workspace } = await setup(
    ["text", "image"],
    ["started", "described"],
  );
  await mkdir(join(resources, "extensions"));
  const record = join(workspace, "observed.json");
  await writeFile(
    join(resources, "extensions", "observer.ts"),
    `import { appendFileSync } from "node:fs";
export default function observer(pi) {
  pi.on("input", (event) => {
    appendFileSync(${JSON.stringify(record)}, JSON.stringify({ input: event.text, images: event.images?.length ?? 0 }) + "\\n");
  });
  pi.registerCommand("note", {
    description: "Record a note",
    handler: async (args) => appendFileSync(${JSON.stringify(record)}, JSON.stringify({ command: args }) + "\\n"),
  });
}
`,
  );
  const store = join(root, "attachments");
  const attachments = await capture(store, [
    { name: "a.png", bytes: png(8, 8) },
    { name: "b.png", bytes: png(9, 9) },
  ]);
  // An established conversation: Pi persists a new session from its first answer.
  await execute(runtime, turn(resources, store, "hello", [], "start"));
  await execute(runtime, turn(resources, store, "/note hello there", attachments, "command"));
  expect(sent).toHaveLength(1);
  await execute(runtime, turn(resources, store, "describe", attachments, "prompt"));
  const observed = (await readFile(record, "utf8"))
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line) as unknown);
  expect(observed).toEqual([
    { input: "hello", images: 0 },
    { command: "hello there" },
    { input: "describe", images: 2 },
  ]);
  expect(images(prompt(sent[1])?.content)).toHaveLength(2);
  // The files sent with the command are recorded too.
  expect(notes(sent[1])).toHaveLength(2);
});

it("applies the inline policy's count and byte limits and leaves other files alone", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-inline-policy-"));
  cleanup.push(() => rm(root, { recursive: true, force: true }));
  const small = [8, 9, 10, 11, 12].map((size) => ({
    name: `${String(size)}.png`,
    bytes: png(size, size),
  }));
  const many = await prepareAttachments(root, await capture(root, small), {
    ...DEFAULT_INLINE_IMAGES,
    max_count: 4,
  });
  expect(many.images).toHaveLength(4);
  expect(many.note).toContain("5. path:");
  expect(many.note).toContain("only 4 images per message are passed inline");

  const large = png(640, 600, true);
  expect(large.length).toBeGreaterThan(1024 * 1024);
  const medium = png(420, 420, true);
  const sized = await prepareAttachments(
    root,
    await capture(root, [
      { name: "large.png", bytes: large },
      { name: "medium-1.png", bytes: medium },
      { name: "medium-2.png", bytes: medium },
    ]),
    { ...DEFAULT_INLINE_IMAGES, max_image_mib: 1, max_total_mib: 1 },
  );
  expect(sized.images).toHaveLength(1);
  expect(sized.note).toContain("larger than 1 MiB");
  expect(sized.note).toContain("inline images may total 1 MiB per message");

  // A 2x2 BMP: an image, but not a format passed inline.
  const bmp = Buffer.alloc(70);
  bmp.write("BM", 0, "ascii");
  bmp.writeUInt32LE(70, 2);
  bmp.writeUInt32LE(54, 10);
  bmp.writeUInt32LE(40, 14);
  bmp.writeInt32LE(2, 18);
  bmp.writeInt32LE(2, 22);
  bmp.writeUInt16LE(1, 26);
  bmp.writeUInt16LE(24, 28);
  const none = await prepareAttachments(
    root,
    await capture(root, [
      { name: "notes.txt", bytes: Buffer.from("text") },
      { name: "empty", bytes: Buffer.alloc(0) },
      { name: "archive.zip", bytes: Buffer.from("PK\u0003\u0004 archive bytes") },
      { name: "program", bytes: Buffer.from("\u007fELF executable bytes") },
      { name: "picture.bmp", bytes: bmp },
    ]),
    DEFAULT_INLINE_IMAGES,
  );
  expect(none.images).toEqual([]);
  // Only PNG, JPEG, WebP and GIF are treated as images; a BMP is a file.
  expect(none.note?.split("\n").filter((line) => line.endsWith("; file"))).toHaveLength(5);
  expect(await prepareAttachments(root, [], DEFAULT_INLINE_IMAGES)).toEqual({ images: [] });
});

it("lets an extension choose the model before Pi prepares the images", async () => {
  const { root, resources, runtime, sent } = await setup(
    ["text", "image"],
    ["chosen"],
    DEFAULT_INLINE_IMAGES,
    [{ id: "reader", input: ["text"] }],
  );
  await mkdir(join(resources, "extensions"));
  await writeFile(
    join(resources, "extensions", "router.ts"),
    `export default function router(pi) {
  pi.on("before_agent_start", async (_event, context) => {
    const reader = context.modelRegistry.find("fixture", "reader");
    if (reader !== undefined) await pi.setModel(reader);
  });
}
`,
  );
  const store = join(root, "attachments");
  const attachments = await capture(store, [{ name: "photo.png", bytes: png(16, 16) }]);
  expect(await execute(runtime, turn(resources, store, "Route this.", attachments))).toBe("chosen");
  // Carillon passed the image; the extension's text-only model received
  // Pi's own placeholder for it.
  const [request] = sent;
  expect(JSON.stringify(request)).toContain('"model":"reader"');
  const message = prompt(request);
  expect(images(message?.content)).toEqual([]);
  expect(text(message?.content)).toContain("image omitted: model does not support images");
});
