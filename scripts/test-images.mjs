// The packaged runtime: attached images reach inference prepared for the
// model by the embedded Photon, and every file is listed. Controlled local
// inference.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { realpathSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { deflateSync } from "node:zlib";
import { startFakeCore } from "../packages/runtime/src/testing.ts";

const release = resolve(
  process.argv[2] ?? assert.fail("usage: node scripts/test-images.mjs RELEASE_DIRECTORY"),
);
const durable = realpathSync(
  resolve(import.meta.dirname, "../packages/runtime-pi/node_modules/@earendil-works/pi-durable"),
);
const photon = dirname(
  createRequire(join(durable, "package.json")).resolve("@silvia-odwyer/photon/package.json"),
);
const namespace = ["--user", "--map-root-user", "--mount"];
const hidePhoton = spawnSync("unshare", [...namespace, "true"]).status === 0;
const root = await mkdtemp(join(tmpdir(), "carillon-packaged-images-"));
const token = "test-only-component-token";
const hash = createHash("sha256").update(token).digest("hex");
const requests = [];
const provider = createServer(async (request, response) => {
  let raw = "";
  for await (const part of request) raw += part;
  requests.push(JSON.parse(raw));
  const chunk = (delta, finish_reason) =>
    `data: ${JSON.stringify({ id: "fixture", object: "chat.completion.chunk", created: 1, model: "vision", choices: [{ index: 0, delta, finish_reason }] })}\n\n`;
  response.writeHead(200, { "content-type": "text/event-stream" });
  response.end(
    `${chunk({ role: "assistant", content: "images checked" }, null)}${chunk({}, "stop")}data: [DONE]\n\n`,
  );
});
provider.listen(0, "127.0.0.1");
await once(provider, "listening");

function png(width, height) {
  const table = Array.from({ length: 256 }, (_, n) => {
    let c = n;
    for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c >>> 0;
  });
  const crc = (bytes) => {
    let c = 0xffffffff;
    for (const byte of bytes) c = table[(c ^ byte) & 0xff] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  };
  const chunk = (type, data) => {
    const body = Buffer.concat([Buffer.from(type), data]);
    const length = Buffer.alloc(4);
    length.writeUInt32BE(data.length);
    const sum = Buffer.alloc(4);
    sum.writeUInt32BE(crc(body));
    return Buffer.concat([length, body, sum]);
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  header.set([8, 2, 0, 0, 0], 8);
  const rows = Buffer.concat(
    Array.from({ length: height }, (_, y) =>
      Buffer.concat([Buffer.from([0]), Buffer.alloc(width * 3, y % 256)]),
    ),
  );
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(rows)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

let runtime;
const core = await startFakeCore();
try {
  const state = join(root, "pi");
  const resources = join(root, "resources");
  const store = join(root, "state", "attachments");
  const capture = "0190a6e0-0000-7000-8000-0000000000b1";
  await mkdir(state, { recursive: true });
  await mkdir(resources);
  await mkdir(join(store, capture), { recursive: true });
  // Larger than the model takes, so it must be decoded and resized.
  const files = [
    { name: "wide.png", bytes: png(2400, 300) },
    { name: "notes.txt", bytes: Buffer.from("plain notes") },
  ];
  for (const [ordinal, file] of files.entries())
    await writeFile(join(store, capture, String(ordinal)), file.bytes);
  await writeFile(
    join(state, "settings.json"),
    JSON.stringify({ defaultProvider: "fixture", defaultModel: "vision" }),
  );
  await writeFile(
    join(state, "models.json"),
    JSON.stringify({
      providers: {
        fixture: {
          baseUrl: `http://127.0.0.1:${provider.address().port}/v1`,
          api: "openai-completions",
          apiKey: "test-only",
          models: [{ id: "vision", input: ["text", "image"] }],
        },
      },
    }),
  );
  const probe = createServer();
  probe.listen(0, "127.0.0.1");
  await once(probe, "listening");
  const url = `http://127.0.0.1:${probe.address().port}`;
  await new Promise((done) => probe.close(done));
  const config = join(root, "carillon.toml");
  const tokenFile = join(root, "runtime.token");
  await writeFile(tokenFile, token, { mode: 0o600 });
  await writeFile(
    config,
    `data_dir = "state"\n[core]\nlisten = "${core.url.replace("http://", "")}"\noperator_token_hash = "${hash}"\n[[agents]]\nid = "default"\nname = "Fixture"\nruntime = "runtime-pi"\nworkspace = ${JSON.stringify(root)}\nresources = "resources"\n[[providers]]\nid = "runtime-pi"\nrole = "runtime"\nversion = "0.0.0"\nurl = "${url}"\ntoken_hash = "${hash}"\nenabled = true\n`,
  );
  let logs = "";
  const program = [
    join(release, "bin/carillon-runtime-pi"),
    "--config",
    config,
    "--provider",
    "runtime-pi",
    "--token-file",
    tokenFile,
    "--state-dir",
    state,
  ];
  // Hide the build tree's Photon in a private mount namespace, where
  // available, so only the copy the executable embeds can serve.
  const [command, ...args] = hidePhoton
    ? [
        "unshare",
        ...namespace,
        "sh",
        "-c",
        'mount -t tmpfs none "$0" && exec "$@"',
        photon,
        ...program,
      ]
    : program;
  runtime = spawn(command, args, {
    cwd: root,
    env: { ...process.env, PI_OFFLINE: "1", PI_CODING_AGENT_DIR: state },
  });
  runtime.stdout.on("data", (part) => {
    logs += part;
  });
  runtime.stderr.on("data", (part) => {
    logs += part;
  });
  const deadline = Date.now() + 20_000;
  while (true) {
    if (runtime.exitCode !== null) throw new Error(logs);
    if (Date.now() > deadline) throw new Error(`Runtime startup deadline: ${logs}`);
    try {
      if ((await fetch(`${url}/health`)).ok) break;
    } catch {}
    await new Promise((done) => setTimeout(done, 50));
  }
  const result = await fetch(`${url}/v1/inbox`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({
      correlation_id: "packaged-images",
      interface_id: "fixture",
      agent_id: "default",
      audience_id: "images",
      input: "",
      resource_root: resources,
      attachment_root: store,
      attachments: files.map((file, ordinal) => ({
        path: `${capture}/${String(ordinal)}`,
        name: file.name,
        size: file.bytes.length,
        sha256: createHash("sha256").update(file.bytes).digest("hex"),
      })),
    }),
    signal: AbortSignal.timeout(30_000),
  });
  assert.equal(result.status, 202, `${await result.clone().text()}\n${logs}`);
  const path = "/v1/runtime/turns/packaged-images/outcome";
  for (const deadline = Date.now() + 30_000; !core.received.some((call) => call.path === path);) {
    if (Date.now() > deadline) throw new Error(`no outcome: ${logs}`);
    await new Promise((done) => setTimeout(done, 50));
  }
  assert.equal(core.received.find((call) => call.path === path).body.output, "images checked");
  assert.equal(requests.length, 1, logs);
  const users = requests[0].messages.filter((message) => message.role === "user");
  const parts = users.flatMap((message) =>
    Array.isArray(message.content) ? message.content : [{ type: "text", text: message.content }],
  );
  const images = parts.filter((part) => part.type === "image_url");
  const text = parts
    .filter((part) => part.type === "text")
    .map((part) => part.text)
    .join("\n");
  assert.equal(images.length, 1, text);
  // Resized to the model's default 2000 pixels with the embedded Photon.
  const prepared = Buffer.from(
    images[0].image_url.url.replace(/^data:image\/png;base64,/u, ""),
    "base64",
  );
  assert.equal(prepared.readUInt32BE(16), 2000);
  assert.equal(prepared.readUInt32BE(20), 250);
  assert.match(text, /name: "wide\.png"; .*sent inline as an image/u);
  assert.match(text, /name: "notes\.txt"; .*; file$/mu);
  console.log(
    `runtime: the packaged runtime prepared an attached image for the model and listed every file (local inference fixture; ${hidePhoton ? "build-tree Photon hidden, embedded copy used" : "build-tree Photon visible: no user namespaces here"})`,
  );
} finally {
  if (runtime && runtime.exitCode === null) {
    const exited = once(runtime, "exit");
    runtime.kill("SIGTERM");
    await exited;
  }
  provider.closeAllConnections();
  await new Promise((done) => provider.close(done));
  await core.close();
  await rm(root, { recursive: true, force: true });
}
