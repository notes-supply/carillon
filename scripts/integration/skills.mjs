/**
 * Skills a person calls by name, through the services together: Pi lists
 * the agent's skills as commands, and a Discord reply invokes only the
 * skills the person's own words name, never one the quote before them names.
 */
import assert from "node:assert/strict";

export async function testSkills(harness) {
  await testRuntimeCommands(harness);
  await testReplyInvocation(harness);
}

/**
 * `/command` reaches the conversation's runtime through core: Pi lists the
 * agent's skills, and without a Discord connection a submission has nowhere
 * to be anchored, so nothing is sent.
 */
async function testRuntimeCommands({ interact, recent }) {
  const listed = await interact("305", null, "command", "command");
  const labels = listed.components[0].components[0].options.map(({ label }) => label);
  assert.ok(labels.includes("/skill:carillon-hooks"), labels.join(", "));
  const before = recent();
  const refused = await interact(
    "305",
    null,
    "command",
    "command",
    [],
    [{ name: "invocation", type: 3, value: "skill:carillon-hooks" }],
  );
  assert.match(refused.content, /^`\/skill:carillon-hooks`: .*Discord connection/u);
  assert.equal(recent(), before, "an unanchored submission is not accepted");
  const outside = await interact("301", null, "command", "command");
  assert.equal(outside.content, "Use /command inside a conversation with the agent.");
}

async function testReplyInvocation({ requestSimulation, event }) {
  const reply = async (id, content) => {
    const response = await requestSimulation({
      ...event(id, "100", "200", "300", null, content),
      reference: {
        kind: "reply",
        author: { person: { id: "202", name: "$carillon-hooks" } },
        sent_at: "2026-11-05T14:00:00+00:00",
        content: "try $carillon-hooks",
        files: ["$carillon-hooks.png"],
      },
    });
    const body = await response.text();
    assert.equal(response.status, 200, body);
    return JSON.parse(body).delivery;
  };
  const said = (delivery) =>
    /^Local provider response \d+: ([\s\S]*)\. native extension/u.exec(delivery.content)?.[1];
  const quote =
    'In reply to a message from "$carillon-hooks" (Discord user 202), sent 2026-11-05T14:00:00+00:00, with files ["$carillon-hooks.png"]:\n> try $carillon-hooks';

  const invoked = said(await reply("skill-reply", "/skill:carillon-hooks guard my pushes"));
  assert.ok(invoked?.startsWith(`${quote}\n\n<skill name="carillon-hooks" location="`), invoked);
  assert.ok(invoked.endsWith("</skill>\n\nguard my pushes"), invoked);
  assert.equal(invoked.match(/<skill name=/gu).length, 1, "one skill, the one the person named");

  const quoted = await reply("skill-quoted", "what does this say?");
  assert.equal(said(quoted), `${quote}\n\nwhat does this say?`, "a quote invokes nothing");

  const unknown = await reply("skill-unknown", "/skill:carillon-hook");
  assert.equal(unknown.turn_state, "interrupted");
  assert.match(
    unknown.content,
    /^No skill is named "carillon-hook"\. The skills are carillon-hooks, carillon-status, conversation-history\.$/u,
  );
}
