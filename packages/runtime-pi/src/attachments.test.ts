import { fauxAssistantMessage, type Message } from "@earendil-works/pi-ai";
import { createNodePhotonImages } from "@earendil-works/pi-durable/images/node";
import type { ImageProcessor } from "@earendil-works/pi-durable/tools";
import type { TurnAttachment } from "@carillon/protocol";
import { createHash, randomBytes } from "node:crypto";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { deflateSync } from "node:zlib";
import { afterEach, expect, it } from "vitest";
import { DEFAULT_INLINE_IMAGES, prepareAttachments, type ModelImages } from "./attachments.ts";
import { startFixture, type Fixture } from "./fixture.ts";

const cleanups: (() => Promise<void>)[] = [];
afterEach(async () => {
  for (const cleanup of cleanups.splice(0)) await cleanup();
});

async function directory(): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "carillon-attachments-"));
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  return root;
}

const passthrough: ImageProcessor = {
  prepare: (bytes, mimeType) =>
    Promise.resolve({ data: Buffer.from(bytes).toString("base64"), mimeType }),
};
const inline: ModelImages = {
  kind: "inline",
  processor: passthrough,
  limits: { maxWidth: 2000, maxHeight: 2000, maxBytes: 4_500_000 },
};

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
  capture = "0190a6e0-0000-7000-8000-0000000000a1",
): Promise<TurnAttachment[]> {
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

it("lists every file, inlines suitable images in order and escapes hostile names", async () => {
  const root = await directory();
  const first = png(64, 48);
  const second = png(32, 32);
  const attachments = await capture(root, [
    { name: "first.png", bytes: first, type: "image/png" },
    { name: "report.pdf", bytes: Buffer.from("%PDF-1.7 not an image"), type: "application/pdf" },
    { name: "evil\nname\u202e.png", bytes: png(1200, 1000), type: "image/png" },
    { name: "second.png", bytes: second },
  ]);
  const prepared = await prepareAttachments(
    root,
    attachments,
    { ...DEFAULT_INLINE_IMAGES, max_megapixels: 1 },
    inline,
  );
  expect(prepared.images).toEqual([
    { type: "image", data: first.toString("base64"), mimeType: "image/png" },
    { type: "image", data: second.toString("base64"), mimeType: "image/png" },
  ]);
  const note = prepared.note ?? "";
  expect(note).toContain("[Carillon attachments]");
  expect(note).toContain(`1. path: ${attachments[0]?.path ?? ""}; name: "first.png"`);
  expect(note.match(/sent inline as an image/gu)).toHaveLength(2);
  expect(note).toMatch(
    /2\. path: \S+; name: "report.pdf"; size: \S+ bytes; declared type: "application\/pdf"; file$/mu,
  );
  expect(note).toContain('name: "evil\\nname\\u{202e}.png"');
  expect(note).toContain(
    "not passed as an inline image: 1.2 megapixels exceeds the 1 megapixel limit",
  );
  expect(note).not.toContain("\u202e");
  expect(note).not.toContain(root);
});

it("applies the inline policy's count and byte limits and leaves other files alone", async () => {
  const root = await directory();
  const small = [8, 9, 10, 11, 12].map((size) => ({
    name: `${String(size)}.png`,
    bytes: png(size, size),
  }));
  const many = await prepareAttachments(
    root,
    await capture(root, small),
    { ...DEFAULT_INLINE_IMAGES, max_count: 4 },
    inline,
  );
  expect(many.images).toHaveLength(4);
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
    inline,
  );
  expect(sized.images).toHaveLength(1);
  expect(sized.note).toContain("larger than 1 MiB");
  expect(sized.note).toContain("inline images may total 1 MiB per message");
  const files = await prepareAttachments(
    root,
    await capture(root, [
      { name: "notes.txt", bytes: Buffer.from("text") },
      { name: "empty", bytes: Buffer.alloc(0) },
      { name: "program", bytes: Buffer.from("\u007fELF executable bytes") },
    ]),
    DEFAULT_INLINE_IMAGES,
    inline,
  );
  expect(files.images).toEqual([]);
  expect(files.note?.split("\n").filter((line) => line.endsWith("; file"))).toHaveLength(3);
  expect(await prepareAttachments(root, [], DEFAULT_INLINE_IMAGES, inline)).toEqual({ images: [] });
});

it("prepares images to the model's limits, and keeps them files where it cannot", async () => {
  const root = await directory();
  const attachments = await capture(root, [{ name: "wide.png", bytes: png(400, 200) }]);
  const photon = await createNodePhotonImages({ worker: false });
  const shrunk = await prepareAttachments(root, attachments, DEFAULT_INLINE_IMAGES, {
    kind: "inline",
    processor: photon,
    limits: { maxWidth: 100, maxHeight: 100, maxBytes: 4_500_000 },
  });
  const [image] = shrunk.images;
  const width = Buffer.from(image?.data ?? "", "base64").readUInt32BE(16);
  expect(width).toBe(100);
  const blind = await prepareAttachments(root, attachments, DEFAULT_INLINE_IMAGES, {
    kind: "none",
  });
  expect(blind.images).toEqual([]);
  expect(blind.note).toContain("not passed as an inline image: the model reads no images");
  const refused = await prepareAttachments(root, attachments, DEFAULT_INLINE_IMAGES, {
    ...inline,
    processor: { prepare: () => Promise.resolve(undefined) },
  });
  expect(refused.note).toContain("it could not be prepared for the model");
});

it("gives the model a turn's files, and their directory from then on", async () => {
  const requests: (readonly Message[])[] = [];
  const answer = () => (context: { messages: Message[] }) => {
    requests.push(context.messages);
    return fauxAssistantMessage("Seen.");
  };
  const agent: Fixture = await startFixture({ answers: [answer(), answer()] });
  cleanups.push(() => agent.close());
  const store = join(agent.componentDirectory, "..", "attachments");
  const picture = png(16, 16);
  const attachments = await capture(store, [{ name: "picture.png", bytes: picture }]);
  await agent.runtime.turns.submit(
    agent.turn("turn-1", "What is this?", { attachment_root: store, attachments }),
  );
  await agent.outcome("turn-1");
  await agent.runtime.turns.submit(agent.turn("turn-2", "And now?", { attachment_root: store }));
  await agent.outcome("turn-2");
  const [first = [], second = []] = requests;
  const message = first.findLast((item) => item.role === "user");
  const [input, note, image, ...rest] =
    message?.role === "user" && typeof message.content !== "string" ? message.content : [];
  expect(input).toEqual({ type: "text", text: "What is this?" });
  expect(note?.type === "text" && note.text).toContain('name: "picture.png"');
  expect(image).toEqual({ type: "image", data: picture.toString("base64"), mimeType: "image/png" });
  expect(rest).toEqual([]);
  const section = (messages: readonly Message[]) =>
    messages.flatMap((item) =>
      item.role === "system" && item.sections?.["attachments"] !== undefined
        ? [item.sections["attachments"]]
        : [],
    );
  expect(section(first)).toEqual([expect.stringContaining(`kept in ${store}.`)]);
  expect(section(second)).toEqual(section(first));
});
