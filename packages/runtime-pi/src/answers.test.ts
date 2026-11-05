import {
  fauxAssistantMessage,
  fauxToolCall,
  type FauxResponseFactory,
} from "@earendil-works/pi-ai";
import { chmod, mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { afterEach, expect, it } from "vitest";
import { heldAnswer, startFixture, textsOf, type Fixture } from "./fixture.ts";

const fixtures: Fixture[] = [];
afterEach(async () => {
  await Promise.all(fixtures.splice(0).map((fixture) => fixture.close()));
});

async function fixture(...args: Parameters<typeof startFixture>) {
  const started = await startFixture(...args);
  fixtures.push(started);
  return started;
}

/** A model call that attaches each path in one tool round. */
function attaching(...paths: string[]) {
  return fauxAssistantMessage(
    paths.map((path) => fauxToolCall("attach", { path })),
    { stopReason: "toolUse" },
  );
}

const reporting: FauxResponseFactory = (context) => {
  const results = context.messages.filter((message) => message.role === "toolResult");
  return fauxAssistantMessage(`Tools: ${results.flatMap(textsOf).join(" | ")}`);
};

it("sends the files the agent attached with its answer, in order and once each", async () => {
  const agent = await fixture({
    answers: [
      attaching("out/chart.png", "data set.csv", "out/chart.png"),
      fauxAssistantMessage("Here is the chart."),
    ],
  });
  await mkdir(join(agent.workspace, "out"));
  await writeFile(join(agent.workspace, "out/chart.png"), "chart");
  await writeFile(join(agent.workspace, "data set.csv"), "a,b\n");
  await agent.runtime.turns.submit(agent.turn("turn-1", "Chart it."));
  expect(await agent.outcome("turn-1")).toMatchObject({
    outcome: "answered",
    output: "Here is the chart.",
    files: [join(agent.workspace, "out/chart.png"), join(agent.workspace, "data set.csv")],
  });
  agent.model.setResponses([fauxAssistantMessage("Nothing else.")]);
  await agent.runtime.turns.submit(agent.turn("turn-2", "Anything else?"));
  expect(await agent.outcome("turn-2")).not.toHaveProperty("files");
});

it("tells the model why a path cannot be attached, and the answer carries none of them", async () => {
  const agent = await fixture({
    answers: [attaching("missing.png", "folder", "private.txt"), reporting],
  });
  await mkdir(join(agent.workspace, "folder"));
  const unreadable = join(agent.workspace, "private.txt");
  await writeFile(unreadable, "secret");
  await chmod(unreadable, 0o000);
  await agent.runtime.turns.submit(agent.turn("turn-1", "Send them."));
  const outcome = await agent.outcome("turn-1");
  expect(outcome).not.toHaveProperty("files");
  const said = outcome.outcome === "answered" ? outcome.output : "";
  const refusals = [
    `There is no file at ${join(agent.workspace, "missing.png")}.`,
    `${join(agent.workspace, "folder")} is not a regular file.`,
    // Root reads any file; an ordinary owner cannot read one with no permissions.
    ...(process.getuid?.() === 0 ? [] : [`${unreadable} cannot be read: permission denied.`]),
  ];
  for (const refusal of refusals) expect(said).toContain(refusal);
});

it("keeps the files attached before a restart for the answer that follows it", async () => {
  const never = new Promise<void>(() => undefined);
  let agent = await fixture({
    durable: true,
    answers: [
      attaching("report.pdf"),
      heldAnswer("lost", never),
      fauxAssistantMessage("Here is the report."),
    ],
  });
  await writeFile(join(agent.workspace, "report.pdf"), "%PDF");
  await agent.runtime.turns.submit(agent.turn("turn-1", "Report."));
  for (let attempt = 0; agent.model.state.callCount < 2 && attempt < 200; attempt += 1)
    await new Promise((resolve) => setTimeout(resolve, 10));
  agent = await agent.restart();
  fixtures.splice(0, 1, agent);
  expect(await agent.outcome("turn-1")).toMatchObject({
    outcome: "answered",
    output: "Here is the report.",
    files: [join(agent.workspace, "report.pdf")],
  });
});
