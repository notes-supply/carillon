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
const BESIDE_THINKING = ["Agent     assistant", "Listens   always", "Replies   threads"];
const CHANNEL = ["Agent    assistant", "Listens  always", "Replies  channel"];
const MODEL = "Model       Local echo";
const BUSY = "Busy        steer · default";
const EXTENSIONS = "Extensions  coding, tool-search, discord, extensions/carillon-context.js";

export async function testSettings(services) {
  const { interact, simulateRoute, settings } = services;
  const root = await interact("302", null, "command", "settings");
  assert.equal(root.content, card("302", INHERITED));
  const [pick, set, ...rest] = customIds(root);
  assert.deepEqual(rest, [], "Reset is offered only for a line the channel sets");
  assert.deepEqual(
    options(root, pick).map(({ label }) => label),
    ["Listens", "Replies", "Model"],
    "the route lines are editable, and Agent with one agent offers nothing else",
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
    card("302", [...BESIDE_THINKING, "Model     Local reasoner", "Thinking  Medium · default"]),
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
    "discord-private",
    "  300  #300             assistant, always, channel, Local echo",
    "  301  #301             assistant, mentions, threads, Local echo",
    "  302  #302             assistant, always, threads, Local reasoner, Low",
    "  305  #305             assistant, always, channel, Local echo",
    "  dm   Direct messages  assistant, Local echo",
  ]);

  await services.restart("runtime");
  const persisted = await interact("302", null, "command", "settings");
  assert.equal(persisted.content, card("302", low), "the ledger outlives the runtime process");
  const reset = await interact("302", null, "component", customIds(persisted)[2]);
  assert.equal(reset.content, root.content, "Reset returns the channel to its agent's model");
  const pinned = [
    "#302",
    ...BESIDE_THINKING,
    "Model     Local reasoner",
    "Thinking  Medium · default",
  ];
  const named = settings("--interface", "discord-private", "302", "model=reasoner");
  assert.equal(named, pinned.join("\n"));
  assert.equal(
    settings("302", "thinking=high", "reset", "thinking"),
    pinned.join("\n"),
    "an option's Reset keeps the channel's model",
  );
  assert.equal(settings("302", "reset", "model"), ["#302", ...INHERITED].join("\n"));

  const held = await services.holdTurn("settings-held");
  assert.equal(
    settings("--agent", "assistant", "model=local/reasoner"),
    agentPage(["Model       Local reasoner", "Thinking    Medium · default"]),
  );
  const released = await held.release();
  assert.match(released.output, /model local\/echo\.$/u, "an admitted turn keeps its model");
  const next = await simulateRoute("settings-next", "305", null, "which model");
  assert.match(next.delivery.content, /model local\/reasoner \(thinking=medium\)\.$/u);
  assert.equal(settings("--agent", "assistant", "reset", "model"), agentPage(["Model"]));
  const unset = await interact("305", null, "command", "settings");
  assert.equal(unset.content, card("305", [...CHANNEL, "Model"]));
  const refused = await simulateRoute("settings-unset", "305", null, "which model");
  assert.equal(refused.delivery.turn_state, "interrupted");
  assert.match(refused.delivery.content, /`carillon settings --agent assistant model=/u);
  assert.equal(settings("--agent", "assistant", "model=local/echo"), agentPage([MODEL]));
  await testAgentResources(services);

  const drawn = await interact("305", null, "command", "settings");
  const unreachable = "The runtime is not reachable right now.";
  await services.stop("runtime");
  try {
    const outage = await interact("305", null, "command", "settings");
    assert.equal(outage.content, card("305", CHANNEL, unreachable));
    assert.deepEqual(
      options(outage, customIds(outage)[0]).map(({ label }) => label),
      ["Listens", "Replies"],
      "the route stays editable while its runtime is out",
    );
    const control = customIds(drawn)[1];
    const clicked = await interact("305", null, "component", control, [
      value(drawn, control, "Local reasoner"),
    ]);
    assert.equal(clicked.content, outage.content, "an outage is not a change meanwhile");
    assert.throws(
      () => settings("305", "thinking=high"),
      /thinking cannot be changed now\. The runtime is not reachable right now\./u,
    );
  } finally {
    await services.start("runtime");
  }

  await testRoutes(services);
  await testRefusals(services);
}

/** Route lines change through the provider, and `/settings` routes a new channel. */
async function testRoutes(services) {
  const { interact, settings, simulateRoute, configPath } = services;
  const file = join(dirname(configPath), "providers/discord-private.toml");
  const saved = await readFile(file);
  const lines = (listens, replies) => [
    "Agent    assistant",
    `Listens  ${listens}`,
    `Replies  ${replies}`,
    "Model    Local echo · default",
  ];
  assert.equal(
    settings("302", "listens=mention"),
    ["#302", ...lines("mentions", "threads")].join("\n"),
  );
  const mentioning = await interact("302", null, "command", "settings");
  assert.equal(mentioning.content, card("302", lines("mentions", "threads")));
  const quiet = await unanswered(services, "routes-quiet", "302", "no mention");
  assert.equal(quiet.status, 204, "an unmentioned message starts no turn");
  const mentioned = await simulateRoute("routes-mention", "302", null, "<@201> which model");
  assert.match(mentioned.delivery.content, /model local\/echo\.$/u, "a mention starts a turn");
  settings("302", "listens=always");
  assert.deepEqual(await readFile(file), saved, "the restored route is the saved file");

  const drawn = await interact("302", null, "command", "settings");
  const [pick] = customIds(drawn);
  const replies = await interact("302", null, "component", pick, [value(drawn, pick, "Replies")]);
  const set = customIds(replies)[1];
  const inChannel = await interact("302", null, "component", set, [value(replies, set, "channel")]);
  assert.equal(inChannel.content, card("302", lines("always", "channel")));
  const first = await simulateRoute("routes-channel-1", "302", null, "first");
  const second = await simulateRoute("routes-channel-2", "302", null, "second");
  assert.equal(first.conversation_id, second.conversation_id, "the channel is one conversation");
  const stale = await interact("302", null, "component", set, [value(replies, set, "threads")]);
  assert.equal(stale.content, card("302", lines("always", "channel"), "Changed meanwhile"));
  settings("302", "replies=thread");
  assert.deepEqual(await readFile(file), saved, "the restored route is the saved file");

  const unrouted = await interact("399", null, "command", "settings");
  assert.equal(unrouted.content, card("399", ["Agent"]));
  const agents = customIds(unrouted)[1];
  const routed = await interact("399", null, "component", agents, [
    value(unrouted, agents, "assistant"),
  ]);
  assert.equal(routed.content, card("399", lines("mentions", "threads")));
  assert.match(settings(), /^ {2}399 +#399 +assistant, mentions, threads, Local echo$/mu);
  await writeFile(file, saved);
  await services.restart("discord");
  assert.doesNotMatch(settings(), /^ {2}399 /mu, "the test route is removed");
}

/** A message the provider may leave unanswered, so its status is the outcome. */
async function unanswered({ corePort, configPath }, eventId, channelId, content) {
  const token = await readFile(join(dirname(configPath), "credentials/discord-private.token"));
  return fetch(`http://127.0.0.1:${String(corePort + 1)}/v1/simulate`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${token.toString().trim()}`,
    },
    body: JSON.stringify({
      event_id: eventId,
      guild_id: "100",
      user_id: "200",
      channel_id: channelId,
      parent_channel_id: null,
      content,
    }),
  });
}

async function testAgentResources({ settings, configPath }) {
  const file = join(dirname(configPath), "agents/default/settings.json");
  const original = await readFile(file, "utf8");
  try {
    assert.equal(
      settings("--agent", "assistant", "busy=queue", "extensions=coding,discord"),
      agentPage([MODEL], ["Busy        queue", "Extensions  coding, discord"]),
    );
    assert.deepEqual(JSON.parse(await readFile(file, "utf8")), {
      extensions: ["coding", "discord"],
      busy: "queue",
    });
    const before = await readFile(file);
    assert.throws(
      () => settings("--agent", "assistant", "extensions=coding,nope"),
      /nope is not a choice of Extensions/u,
    );
    assert.deepEqual(await readFile(file), before, "a refused extension writes nothing");
    assert.equal(
      settings("--agent", "assistant", "reset", "busy", "reset", "extensions"),
      agentPage([MODEL], [BUSY, "Extensions  coding, tool-search, discord · default"]),
    );
    assert.deepEqual(JSON.parse(await readFile(file, "utf8")), {});
  } finally {
    await writeFile(file, original);
  }
  assert.equal(settings("--agent", "assistant"), agentPage([MODEL]));
}

function agentPage(models, resources = [BUSY, EXTENSIONS]) {
  return ["assistant", ...models, ...resources].join("\n");
}

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
      [{ channel_id: "499", parent_channel_id: "399" }, "threads outside a route must fail closed"],
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
    const unrouted = await interact("399", null, "command", "settings");
    assert.equal(unrouted.content, card("399", ["Agent"]));
    assert.deepEqual(calls, ["POST"], "an unrouted channel's page asks no runtime");
  } finally {
    await services.stop("discord");
    await services.start("discord");
    await rm(counted, { force: true });
    proxy.closeAllConnections();
    await new Promise((done) => proxy.close(done));
  }
}

function card(channel, lines, notice) {
  const warning = notice === undefined ? "" : `⚠ ${notice}\n`;
  return `${warning}<#${channel}>\n\`\`\`\n${lines.join("\n")}\n\`\`\``;
}
