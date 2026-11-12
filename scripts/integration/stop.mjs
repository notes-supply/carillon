/**
 * A person stops a running answer, through the services together: Pi lists
 * `stop` on the running message's turn, core relays the interface's control
 * of the message's event to the runtime, and the turn ends stopped, with a
 * silence in place of an answer.
 */
import assert from "node:assert/strict";
import { rm } from "node:fs/promises";
import { join } from "node:path";

/** The job whose newest turn shows a running call, once core's job read has one. */
export async function runningJob(jobs) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const read = await jobs();
    assert.equal(read.status, 200);
    const running = read.body.jobs.find((job) => job.turns.at(-1).activity?.running.length > 0);
    if (running !== undefined) return running;
    await new Promise((resolveWait) => setTimeout(resolveWait, 25));
  }
  return assert.fail("the running command never became visible");
}

/**
 * Expects an agent whose answer to a message runs a command that waits for
 * the file `marker` in `workspace`. `discord` is the Discord provider's
 * authorization with core.
 */
export async function testRunningStop({ basePort, discord, simulate, jobs, workspace, marker }) {
  const control = async () => {
    const url = `http://127.0.0.1:${String(basePort)}/v1/events/activity-stop/control`;
    const response = await fetch(url, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-carillon-provider": "discord-private",
        authorization: discord,
      },
      body: JSON.stringify({ action: "stop" }),
    });
    return { status: response.status, body: await response.text() };
  };
  await rm(join(workspace, marker));
  const stopped = simulate("activity-stop", "wait for the marker again");
  stopped.catch(() => undefined);
  const running = await runningJob(() => jobs(discord));
  const turn = running.turns.at(-1);
  assert.equal(turn.state, "running");
  assert.deepEqual(turn.activity.controls, ["stop"]);

  const taken = await control();
  assert.equal(taken.status, 204, taken.body);
  const { receipt, delivery, messages } = await stopped;
  assert.equal(receipt.turn_id, turn.turn_id);
  assert.equal(delivery.silence, "stopped");
  assert.equal(messages, undefined, "a stopped turn sends nothing");
  assert.deepEqual((await jobs(discord)).body, { jobs: [] });
  const again = await control();
  assert.equal(again.status, 409, again.body);
}
