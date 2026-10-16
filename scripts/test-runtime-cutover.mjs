import assert from "node:assert/strict";

export async function testChannelCutover({ fake, simulateRoute, opened }) {
  const runs = () => [...fake.threads.values()].flatMap((thread) => thread.runs).length;
  const before = runs();
  const notice = await simulateRoute("cutover-notice", "300", null, "continue here");
  assert.equal(notice.receipt.state, "interrupted");
  assert.equal(
    notice.delivery.content,
    "This conversation ran on runtime-pi, which no longer runs assistant. Your next message starts a new conversation on runtime-t3.",
  );
  assert.equal(runs(), before, "the cutover notice must not dispatch native work");
  let thread;
  for (const id of ["cutover-next", "cutover-stays"]) {
    const pending = simulateRoute(id, "300", null, "continue here");
    const run = await opened();
    thread ??= run.thread;
    assert.equal(run.thread, thread, "later messages keep the new native conversation");
    fake.assistant(run.thread, run.run, "Now on T3.");
    fake.complete(run.thread, run.run);
    const reply = await pending;
    assert.equal(reply.conversation_id, notice.conversation_id);
    assert.equal(reply.receipt.state, "queued");
    assert.equal(reply.delivery.turn_state, "completed");
    assert.equal(reply.delivery.content, "Now on T3.");
  }
  const duplicate = await simulateRoute("cutover-notice", "300", null, "continue here");
  assert.equal(duplicate.receipt.duplicate, true);
  assert.equal(duplicate.receipt.turn_id, notice.receipt.turn_id);
  assert.equal(duplicate.receipt.state, "interrupted");
}
