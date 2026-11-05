/**
 * Files through the services together: a message's attachments, which core
 * captures before its turn runs, and the files an agent attaches to its
 * answer, which core keeps and the Discord provider reads to send.
 */
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, readdir, readFile, rm, stat, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";

/** Answers with files go to a channel of their own, apart from the conversations counted elsewhere. */
const CHANNEL = "305";

/** Returns the turn of the message whose attachments core captured. */
export async function testFiles(harness) {
  const turn = await testAttachments(harness);
  await testAnswerFiles(harness);
  await testDirectMessageFiles(harness);
  await testAnswerFilesOutlastRestarts(harness);
  return turn;
}

/** Files staged as the Discord provider stages them, captured through core. */
async function testAttachments(harness) {
  const {
    basePort,
    installation,
    event,
    requestSimulation,
    credential,
    credentialValue,
    waitForTurn,
    waitForDeliveryClaim,
    acknowledgeDelivery,
  } = harness;
  // Simulation fetches an admitted message's files only from Discord's CDN:
  // the built provider refuses anything else before a request is made.
  const local = await requestSimulation({
    ...event("attachments-local", "100", "200", "300", null, ""),
    attachments: [
      {
        id: "601",
        filename: "local.png",
        size: 4,
        url: `http://127.0.0.1:${String(basePort)}/attachments/300/601/local.png`,
      },
    ],
  });
  const refusal = await local.text();
  assert.equal(local.status, 422, refusal);
  assert.match(refusal, /not a Discord attachment/u);
  const incoming = join(installation, "state/components/discord-private/incoming");
  const report = Buffer.from("%PDF-1.7 integration bytes");
  await mkdir(join(incoming, "batch"), { recursive: true });
  await writeFile(join(incoming, "batch", "0"), report);
  await writeFile(join(incoming, "batch", "1"), "notes");
  // Staging that links elsewhere is refused before anything is copied.
  await mkdir(join(incoming, "linked"));
  await symlink(credential("operator"), join(incoming, "linked", "0"));
  const operatorToken = await stat(credential("operator"));
  const linked = await postCoreEvent(harness, "attachments-linked", "", {
    batch: "linked",
    files: [{ name: "token", size: operatorToken.size }],
  });
  assert.equal(linked.status, 400, await linked.text());
  const store = join(installation, "state/attachments");
  assert.deepEqual(
    await readdir(store).catch(() => []),
    [],
    "a refused capture left a copy behind",
  );
  const response = await postCoreEvent(harness, "attachments-1", "", {
    batch: "batch",
    files: [
      { name: "report.pdf", size: report.length, content_type: "application/pdf" },
      { name: "notes.txt", size: 5 },
    ],
  });
  const body = await response.text();
  assert.equal(response.status, 200, body);
  const receipt = JSON.parse(body);
  // The interface's staging is its own; accepted files no longer need it.
  await rm(incoming, { recursive: true });
  const inspection = await waitForTurn(receipt.turn_id, "completed", "pending");
  assert.match(inspection.output, /attachment report\.pdf 26 bytes;/u);
  assert.match(inspection.output, /attachment notes\.txt 5 bytes\./u);
  assert.equal(inspection.attachments.length, 2);
  assert.match(inspection.attachments[0].path, /^[0-9a-f-]{36}\/0$/u);
  // Asked about the event again, core names the same accepted turn, in the
  // state it has now reached.
  const status = await fetch(`http://127.0.0.1:${String(basePort)}/v1/events/attachments-1`, {
    headers: {
      authorization: `Bearer ${await credentialValue("discord-private")}`,
      "x-carillon-provider": "discord-private",
    },
  });
  assert.deepEqual(await status.json(), {
    status: "accepted",
    ...receipt,
    state: "completed",
    duplicate: true,
  });
  const delivery = await waitForDeliveryClaim("attachments-check");
  assert.equal(delivery.turn_id, receipt.turn_id);
  await acknowledgeDelivery(delivery.id, "attachments-check");
  return receipt.turn_id;
}

async function postCoreEvent({ basePort, credentialValue }, eventId, content, attachments) {
  return await fetch(`http://127.0.0.1:${String(basePort)}/v1/events`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${await credentialValue("discord-private")}`,
      "x-carillon-provider": "discord-private",
      "content-type": "application/json",
    },
    body: JSON.stringify({
      event_id: eventId,
      agent_id: "assistant",
      user_id: "200",
      audience_id: "discord-100-306",
      channel_id: "306",
      delivery_target_id: "306",
      feedback_target_id: `306:${eventId}`,
      content,
      attachments,
    }),
  });
}

/**
 * Deterministic Pi attaches files with its `attach` tool; core keeps a copy
 * of each Discord can upload with the answer, and the Discord provider
 * composes the messages that send them from those copies, checked against
 * their digests.
 */
async function testAnswerFiles({ installation, workspace, simulateRoute }) {
  const simulate = (eventId, content) => simulateRoute(eventId, CHANNEL, null, content);
  const store = join(installation, "state/attachments");
  const chart = join(workspace, "chart.png");
  const talk = join(workspace, "talk.bin");
  const drawn = Buffer.from("\u0089PNG integration chart");
  await writeFile(chart, drawn);
  await writeFile(talk, Buffer.alloc(21 * 1024 * 1024));
  const { delivery, messages } = await simulate("answer-files", `attach ${chart} ${talk}`);
  assert.equal(delivery.turn_state, "completed");
  assert.deepEqual(
    delivery.files.map(({ name, source }) => [name, source]),
    [["chart.png", chart]],
  );
  assert.deepEqual(await readFile(join(store, delivery.files[0].path)), drawn);
  assert.deepEqual(
    await readdir(join(store, delivery.files[0].path, "..")),
    ["0"],
    "core keeps no copy of a file Discord cannot upload",
  );
  assert.equal(messages.length, 1, "the answer and its chart are one message");
  assert.deepEqual(messages[0].files, [["chart.png", sha256(drawn)]]);
  assert.ok(
    messages[0].content.endsWith(
      `Could not attach ${talk}: it has 21 MiB, over the 20 MiB this channel uploads of one file.`,
    ),
    messages[0].content,
  );

  const missing = join(workspace, "missing.png");
  const refused = await simulate("answer-file-missing", `attach ${missing}`);
  assert.equal(refused.delivery.files, undefined);
  assert.ok(
    refused.delivery.content.includes(`There is no file at ${missing}.`),
    refused.delivery.content,
  );
}

/** An answer in a person's direct messages carries its files to them too. */
async function testDirectMessageFiles({ installation, workspace, event, requestSimulation }) {
  const notes = join(workspace, "dm-notes.txt");
  await writeFile(notes, "for your eyes");
  const response = await requestSimulation(
    event("dm-answer-files", null, "200", "700", null, `attach ${notes}`),
  );
  const body = await response.text();
  assert.equal(response.status, 200, body);
  const { conversation_id: conversation, delivery, messages } = JSON.parse(body);
  assert.equal(conversation, "discord-dm-700");
  assert.equal(delivery.target_id, "700");
  assert.equal(delivery.agent_id, "assistant");
  assert.deepEqual(
    delivery.files.map(({ name, source }) => [name, source]),
    [["dm-notes.txt", notes]],
  );
  const store = join(installation, "state/attachments");
  assert.equal(await readFile(join(store, delivery.files[0].path), "utf8"), "for your eyes");
  assert.deepEqual(
    messages.map(({ files }) => files),
    [[["dm-notes.txt", sha256("for your eyes")]]],
  );
}

/**
 * An answer recorded while Discord is down keeps its file across a core
 * restart, after the agent's own copy is gone: each claim names core's copy,
 * and nothing runs again.
 */
async function testAnswerFilesOutlastRestarts(harness) {
  const { installation, workspace, submitCoreEvent, waitForTurn } = harness;
  const { waitForDeliveryClaim, acknowledgeDelivery, stop, start, restart } = harness;
  const store = join(installation, "state/attachments");
  await stop("discord");
  const report = join(workspace, "report.pdf");
  await writeFile(report, "%PDF-1.7 report");
  const pending = await submitCoreEvent("answer-file-retry", `attach ${report}`, CHANNEL);
  const answered = await waitForTurn(pending.turn_id, "completed", "pending");
  await rm(report);
  await restart("core");
  const claimed = await waitForDeliveryClaim("answer-file-retry");
  const again = await waitForDeliveryClaim("answer-file-retry");
  assert.equal(again.id, claimed.id);
  assert.deepEqual(again.files, claimed.files);
  assert.equal(await readFile(join(store, claimed.files[0].path), "utf8"), "%PDF-1.7 report");
  await acknowledgeDelivery(claimed.id, "answer-file-retry");
  const delivered = await waitForTurn(pending.turn_id, "completed", "delivered");
  assert.equal(delivered.output, answered.output);
  assert.deepEqual(delivered.answer_files, claimed.files);
  await start("discord");
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}
