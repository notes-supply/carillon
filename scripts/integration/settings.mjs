/**
 * `/settings` and `carillon settings` against the running services: a
 * routed channel's page drawn alike at its root, in its threads and at the
 * terminal, changes that reach the next turn of every conversation in that
 * channel and no other, and refusals that never reach core. Every value it
 * changes is restored.
 */
import assert from "node:assert/strict";
import { once } from "node:events";
import { readFile, rm, writeFile } from "node:fs/promises";
import { createServer, request as forward } from "node:http";
import { dirname, join } from "node:path";
import { customIds, options, value } from "./replies.mjs";

const INHERITED = [
  "Agent    assistant",
  "Listens  always",
  "Replies  threads",
  "Model    Local echo · default",
];
/** 302's route lines, padded to the width a Thinking line sets. */
const BESIDE_THINKING = ["Agent     assistant", "Listens   always", "Replies   threads"];
const CHANNEL = ["Agent    assistant", "Listens  always", "Replies  channel"];

export async function testSettings(services) {
  const { interact, simulateRoute, settings } = services;
  const root = await interact("302", null, "command", "settings");
  assert.equal(root.content, card("302", INHERITED));
  const [pick, set, ...rest] = customIds(root);
  assert.deepEqual(rest, [], "Reset is offered only for a line the channel sets");
  assert.deepEqual(
    options(root, pick).map(({ label }) => label),
    ["Model"],
    "the route lines are read-only",
  );
  assert.deepEqual(
    options(root, set).map(({ label, description, default: held }) => [label, description, held]),
    [
      ["Local echo", "Local", true],
      ["Local reasoner", "Local", false],
      ["Substitute fixed", "Substitute", false],
    ],
  );
  const conversation = await simulateRoute("501", "302", null, "which model");
  assert.match(conversation.delivery.content, /model local\/echo\.$/u);
  const threaded = await interact("501", "302", "command", "settings");
  assert.equal(threaded.content, root.content, "a thread shows its channel's page");

  const click = (reply, index, label) => {
    const control = customIds(reply)[index];
    return interact("501", "302", "component", control, [value(reply, control, label)]);
  };
  const reasoner = await click(threaded, 1, "Local reasoner");
  assert.equal(
    reasoner.content,
    card("302", [...BESIDE_THINKING, "Model     Local reasoner", "Thinking  Medium"]),
  );
  const thinking = await click(reasoner, 0, "Thinking");
  assert.deepEqual(
    options(thinking, customIds(thinking)[1]).map(({ label }) => label),
    ["Off", "Low", "Medium", "High"],
  );
  const high = await click(thinking, 1, "High");
  assert.equal(
    high.content,
    card("302", [...BESIDE_THINKING, "Model     Local reasoner", "Thinking  High"]),
  );
  const existing = await simulateRoute("settings-existing", "501", "302", "which model");
  const started = await simulateRoute("502", "302", null, "which model");
  assert.equal(existing.conversation_id, conversation.conversation_id);
  assert.notEqual(started.conversation_id, existing.conversation_id);
  for (const { delivery } of [existing, started])
    assert.match(delivery.content, /model local\/reasoner \(thinking=high\)\.$/u);
  const elsewhere = await simulateRoute("settings-elsewhere", "305", null, "which model");
  assert.match(elsewhere.delivery.content, /model local\/echo\.$/u, "305 keeps the agent's model");

  const low = [...BESIDE_THINKING, "Model     Local reasoner", "Thinking  Low"];
  assert.equal(settings("302", "thinking=low"), ["#302", ...low].join("\n"));
  assert.equal((await interact("302", null, "command", "settings")).content, card("302", low));
  const before = await readFile(services.ledger);
  const stale = await click(high, 1, "Medium");
  assert.equal(stale.content, card("302", low, "Changed meanwhile"));
  assert.deepEqual(await readFile(services.ledger), before, "a stale click writes nothing");
  assert.deepEqual(settings().split("\n"), [
    "300  #300  assistant, always, channel, Local echo",
    "301  #301  assistant, mentions, threads, Local echo",
    "302  #302  assistant, always, threads, Local reasoner, Low",
    "305  #305  assistant, always, channel, Local echo",
  ]);

  await services.restart("runtime");
  const persisted = await interact("302", null, "command", "settings");
  assert.equal(persisted.content, card("302", low), "the ledger outlives the runtime process");
  const reset = await interact("302", null, "component", customIds(persisted)[2]);
  assert.equal(reset.content, root.content, "Reset returns the channel to its agent's model");
  assert.equal(
    settings("302", "model=reasoner"),
    ["#302", ...BESIDE_THINKING, "Model     Local reasoner", "Thinking  Medium"].join("\n"),
  );
  assert.equal(settings("302", "reset", "thinking"), ["#302", ...INHERITED].join("\n"));

  const held = await services.holdTurn("settings-held");
  assert.equal(
    settings("--agent", "assistant", "model=local/reasoner"),
    "assistant\nModel     Local reasoner\nThinking  Medium",
  );
  const released = await held.release();
  assert.match(released.output, /model local\/echo\.$/u, "an admitted turn keeps its model");
  const next = await simulateRoute("settings-next", "305", null, "which model");
  assert.match(next.delivery.content, /model local\/reasoner \(thinking=medium\)\.$/u);
  assert.equal(settings("--agent", "assistant", "reset", "model"), "assistant\nModel");
  const unset = await interact("305", null, "command", "settings");
  assert.equal(unset.content, card("305", [...CHANNEL, "Model"]));
  const refused = await simulateRoute("settings-unset", "305", null, "which model");
  assert.equal(refused.delivery.turn_state, "interrupted");
  assert.match(refused.delivery.content, /`carillon settings --agent assistant model=/u);
  assert.equal(
    settings("--agent", "assistant", "model=local/echo"),
    "assistant\nModel  Local echo",
  );

  await services.stop("runtime");
  try {
    const outage = await interact("305", null, "command", "settings");
    assert.equal(outage.content, card("305", CHANNEL, "The runtime is not reachable right now."));
    assert.deepEqual(outage.components, []);
  } finally {
    await services.start("runtime");
  }

  await testRefusals(services);
}

/**
 * Interactions are admitted like messages: a refused one never reaches the
 * module, so core receives no settings call for it. The Discord provider
 * reaches core through a proxy that counts them, and an admitted
 * `/settings` shows that the count sees its calls.
 */
async function testRefusals(services) {
  const { interact, requestInteraction, interaction } = services;
  const calls = [];
  const proxy = createServer((request, response) => {
    if (request.url === "/v1/settings") calls.push(request.method);
    const upstream = forward(
      {
        port: services.corePort,
        method: request.method,
        path: request.url,
        headers: request.headers,
      },
      (answer) => {
        response.writeHead(answer.statusCode, answer.headers);
        answer.pipe(response);
      },
    );
    upstream.on("error", () => response.destroy());
    request.pipe(upstream);
  });
  proxy.listen(0, "127.0.0.1");
  await once(proxy, "listening");
  const registry = await readFile(services.configPath, "utf8");
  const counted = join(dirname(services.configPath), "discord-counted.toml");
  const proxied = registry.replace(
    `listen = "127.0.0.1:${String(services.corePort)}"`,
    `listen = "127.0.0.1:${String(proxy.address().port)}"`,
  );
  assert.notEqual(proxied, registry);
  await writeFile(counted, proxied);
  await services.stop("discord");
  try {
    await services.start("discord", { config: counted });
    await interact("305", null, "command", "settings");
    assert.deepEqual(calls, ["POST"], "an admitted /settings reads through core");
    for (const [changes, message] of [
      [{ guild_id: "999" }, "other guilds must fail closed"],
      [{ guild_id: null }, "direct messages must fail closed"],
      [{ user_id: "999" }, "users outside the allowlist must fail closed"],
      [{ channel_id: "399" }, "unconfigured channels must fail closed"],
    ]) {
      const response = await requestInteraction({
        ...interaction("305", null, "command", "settings", []),
        ...changes,
      });
      assert.equal(response.status, 403, message);
    }
    const unauthenticated = await requestInteraction(
      interaction("305", null, "command", "settings", []),
      "operator",
    );
    assert.equal(unauthenticated.status, 403, "interactions require the provider credential");
    assert.deepEqual(calls, ["POST"], "a refused interaction reaches no settings");
  } finally {
    await services.stop("discord");
    await services.start("discord");
    await rm(counted, { force: true });
    proxy.closeAllConnections();
    await new Promise((done) => proxy.close(done));
  }
}

/** A `/settings` reply's text: its notice, the channel, then the page's lines. */
function card(channel, lines, notice) {
  const warning = notice === undefined ? "" : `⚠ ${notice}\n`;
  return `${warning}<#${channel}>\n\`\`\`\n${lines.join("\n")}\n\`\`\``;
}
