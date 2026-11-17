import type { Locator } from "@playwright/test";
import { arrive, atWork, clockAt, expect, open, send, test, turns } from "./fixtures.ts";

test("opening a conversation shows what was said and what was answered", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Plan the release.");
  await expect(turns(page)).toHaveCount(1);
  await expect(turns(page).getByText("Plan the release.")).toHaveText(
    /^Plan the release\.\nStart with the notes\.$/u,
    { useInnerText: true },
  );
  await expect(turns(page).first()).toContainText("I will start with the notes.");
  await expect(turns(page).first()).toContainText("Answered 11:40, 8 s");
  await expect(page).toHaveTitle("Plan the release.");
  await open(page, "Chart last week's numbers.");
  await expect(turns(page)).toHaveCount(2);
  await expect(turns(page).getByText("I will start with the notes.")).toHaveCount(0);
  await expect(page).toHaveTitle("Chart last week's numbers.");
});

test("a conversation of files only is named so, and its file downloads", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Files only");
  const file = turns(page).first().getByRole("link", { name: "numbers.csv" });
  await expect(file).toContainText("10 B");
  await expect(file).toHaveAttribute("download", "numbers.csv");
});

test("a turn at work shows what its runtime is doing, as that changes", async ({ page, fake }) => {
  await arrive(page, fake, "turn");
  await open(page, "Plan the release.");
  await clockAt(page, 30);
  await send(page, "Write the release notes.");
  const turn = turns(page).nth(1);
  await expect(atWork(turn)).toContainText("Waiting its turn");
  await expect(atWork(turn)).toContainText("30 s");

  await fake.step("core hands the turn to its runtime");
  await expect(atWork(turn)).toContainText("Starting");

  await fake.step("the model is asked");
  await expect(atWork(turn)).toContainText("Waiting for the model");
  await expect(atWork(turn)).toContainText("Reading the release notes.");

  await fake.step("a tool call is under way");
  await expect(atWork(turn)).toContainText("Using tools");
  await expect(atWork(turn).getByRole("img", { name: "1 tool call, 1 under way" })).toBeVisible();
  await expect(atWork(turn)).toContainText("1 tool call, 30 s");
  await expect(atWork(turn).getByRole("listitem")).toContainText(["read docs/RELEASE.md 24 s"], {
    useInnerText: true,
  });

  await fake.step("one call is done, one failed and one is under way");
  await expect(atWork(turn)).toContainText("Checking the tests before the notes.");
  await expect(
    atWork(turn).getByRole("img", { name: "3 tool calls, 1 under way, 1 failed, 1 finished" }),
  ).toBeVisible();
  await expect(atWork(turn).getByRole("listitem")).toContainText(
    ["read docs/RELEASE.md 3 s", "bash cargo test failed", "bash git log -5 about 21 s"],
    { useInnerText: true },
  );

  await fake.step("work is delegated, with notes");
  await expect(atWork(turn)).toContainText("Comparing the changelog with the commits.");
  await expect(atWork(turn).getByRole("listitem")).toContainText(
    [
      "read docs/RELEASE.md 3 s",
      "bash cargo test failed",
      "bash git log -5 41 ms",
      "Delegated Check the changelog (scout) running, 17 s",
      "Task Draft the summary queued",
    ],
    { useInnerText: true },
  );
  await expect(atWork(turn)).toContainText("2 more not shown.");
  await expect(atWork(turn)).toContainText("gpt-6.1 high, context 5% full");

  await fake.step("the turn ends with an answer and two files");
  await expect(atWork(turn)).toHaveCount(0);
  await expect(turn).toContainText("Answered 12:00, 22 s, 4 tool calls");
});

test("an answer is shown as the provider rendered it, with its files", async ({ page, fake }) => {
  await arrive(page, fake, "turn");
  await open(page, "Plan the release.");
  await send(page, "Write the release notes.");
  await expect(turns(page)).toHaveCount(2);
  for (const label of [
    "core hands the turn to its runtime",
    "the model is asked",
    "a tool call is under way",
    "one call is done, one failed and one is under way",
    "work is delegated, with notes",
    "the turn ends with an answer and two files",
  ])
    await fake.step(label);
  const turn = turns(page).nth(1);
  await expect(turn.getByRole("heading", { name: "Release plan" })).toBeVisible();
  await expect(turn.locator("strong", { hasText: "ready" })).toBeVisible();
  const link = turn.getByRole("link", { name: "the last release" });
  await expect(link).toHaveAttribute("href", "https://example.com/changelog");
  await expect(link).toHaveAttribute("target", "_blank");
  await expect(turn.getByRole("cell", { name: "58" })).toBeVisible();
  await expect(turn.locator("pre code")).toHaveText(/carillon restart/u);
  await expect(turn).toContainText("<script>alert(1)</script> is shown as text.");
  const picture = turn.getByRole("img", { name: "chart.png" });
  await expect(picture).toBeVisible();
  await expect(picture).toHaveJSProperty("naturalWidth", 1);
  await expect(turn).toContainText("chart.png, 70 B");
  const notes = turn.getByRole("link", { name: "notes.txt" });
  await expect(notes).toContainText("15 B");
  await expect(notes).toHaveAttribute("download", "notes.txt");
});

test("each ended turn says how it ended", async ({ page, fake }) => {
  await arrive(page, fake, "endings");
  await open(page, "Summarise the meeting.");
  await expect(turns(page)).toHaveCount(8);
  await expect(turns(page).nth(0)).toContainText("Did not finish 11:45, 4 s");
  await expect(turns(page).nth(0)).toContainText(
    "Carillon restarted before this turn finished. Send the message again.",
  );
  await expect(turns(page).nth(1)).toContainText(
    "A later message joined this one. Their answer is below, on the newest message that took part.",
  );
  await expect(turns(page).nth(2)).toContainText("The report has both weeks.");
  await expect(turns(page).nth(2)).toContainText("Answered 11:48, 1 min 35 s, 4 tool calls");
  await expect(turns(page).nth(3)).toContainText("Stopped 11:50, 12 s");
  await expect(turns(page).nth(4)).toContainText("Scheduled run: morning summary");
  await expect(turns(page).nth(4)).toContainText("Opened by Carillon, not by a message.");
  await expect(turns(page).nth(4)).toContainText("Nothing new this morning.");
  await expect(turns(page).nth(5)).toContainText("A turn of its own");
  await expect(turns(page).nth(5)).toContainText("Opened by Carillon, not by a message.");
});

test("a running turn whose runtime says nothing is at work, and a queued one waits", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "endings");
  await open(page, "Summarise the meeting.");
  await expect(atWork(turns(page).nth(6))).toContainText("At work");
  await expect(atWork(turns(page).nth(6))).toContainText("1 min 0 s");
  await expect(atWork(turns(page).nth(7))).toContainText("Waiting its turn");
  await expect(atWork(turns(page).nth(7))).toContainText("30 s");
});

test("the time a turn has been at work moves on without a new snapshot", async ({ page, fake }) => {
  await arrive(page, fake, "endings");
  await open(page, "Summarise the meeting.");
  await expect(atWork(turns(page).nth(6))).toContainText("1 min 0 s");
  await clockAt(page, 65);
  await expect(atWork(turns(page).nth(6))).toContainText("2 min 5 s");
  await expect(atWork(turns(page).nth(7))).toContainText("1 min 35 s");
});

test("what a turn did is folded behind its ending, and stays unfolded while another turn changes", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "endings");
  await open(page, "Summarise the meeting.");
  const answered = turns(page).nth(2);
  await expect(answered.getByText("git log -5")).toBeHidden();
  await answered.getByText("Answered 11:48").click();
  await expect(answered.getByText("git log -5")).toBeVisible();
  await expect(answered.getByRole("listitem")).toContainText(
    [
      "read docs/RELEASE.md 3 s",
      "bash cargo test failed",
      "bash git log -5 41 ms",
      "write notes.txt 12 ms",
    ],
    { useInnerText: true },
  );
  await fake.step("the running turn reports its activity");
  await expect(atWork(turns(page).nth(6))).toContainText("Thinking about what is next.");
  await expect(answered.getByText("git log -5")).toBeVisible();
});

test("the transcript follows its end, and stays where a person scrolled to", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "long");
  await open(page, "Message 1");
  await expect(turns(page)).toHaveCount(30);
  await expect(turns(page).last().getByText("Message 30", { exact: true })).toBeInViewport();
  await fake.step("a first turn arrives");
  await expect(page.getByText("First arrival.")).toBeInViewport();

  await turns(page).first().getByText("Message 1", { exact: true }).scrollIntoViewIfNeeded();
  await expect(turns(page).first().getByText("Message 1", { exact: true })).toBeInViewport();
  await fake.step("a second turn arrives");
  await expect(page.getByText("Second arrival.")).toBeAttached();
  // Two frames, so a page that wrongly scrolls after it draws has done so before the check.
  await page.evaluate(
    "new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)))",
  );
  await expect(turns(page).first().getByText("Message 1", { exact: true })).toBeInViewport();
  await expect(page.getByText("Second arrival.")).not.toBeInViewport();
});

test("earlier turns are shown on request, until there are none before them", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "earlier");
  await open(page, "Message 1");
  const earlier = page.getByRole("button", { name: "Show earlier turns" });
  await expect(turns(page)).toHaveCount(50);
  await expect(turns(page).first()).toContainText("Message 71");
  await earlier.click();
  await expect(turns(page)).toHaveCount(100);
  await expect(turns(page).first()).toContainText("Message 21");
  await expect(turns(page).last()).toContainText("Answer 120.");
  await earlier.click();
  await expect(turns(page)).toHaveCount(120);
  await expect(turns(page).first().getByText("Message 1", { exact: true })).toBeVisible();
  await expect(earlier).toBeHidden();
});

test("hovering a time shows its full date", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Plan the release.");
  const said = turns(page).first().getByText("11:40").first();
  await expect(said).toHaveAttribute("title", "15/11/2026, 11:40:00");
  await expect(said).toHaveAttribute("datetime", "2026-11-15T11:40:00.000Z");
  const answered = turns(page).first().getByText("11:40").last();
  await expect(answered).toHaveAttribute("title", "15/11/2026, 11:40:08");
});

test("the marks of a turn at work say what each stands for", async ({ page, fake }) => {
  await arrive(page, fake, "controls");
  await open(page, "Write the long report.");
  await expect(
    atWork(turns(page).first()).getByRole("img", { name: "5 tool calls" }),
  ).toHaveAttribute(
    "title",
    "One mark for each tool call: a dot for a call no longer listed, a bar for one that finished, a short bar for one that failed, a hollow bar for one under way. The ring is the turn itself.",
  );
});

test("a turn at work shows at most 48 dots for calls no longer listed, so a hundred snapshots more add no element to the page", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "large");
  await open(page, "Message 1");
  const turn = atWork(turns(page).last());
  const dots = (calls: number): Locator =>
    turn.getByRole("img", { name: `${String(calls)} tool calls` }).locator("i.past");
  for (let tick = 1; tick <= 48; tick += 1) await fake.step(`tick ${String(tick)}`);
  await expect(dots(48)).toHaveCount(48);
  const elements = await page.locator("*").count();

  for (let tick = 49; tick <= 148; tick += 1) await fake.step(`tick ${String(tick)}`);
  await expect(dots(148)).toHaveCount(48);
  expect(await page.locator("*").count()).toBe(elements);
});

test("a newest turn taller than the window shows from its top", async ({ page, fake }) => {
  await arrive(page, fake, "long");
  await open(page, "Message 1");
  const newest = turns(page).last();
  await expect(newest.getByText("Message 30", { exact: true })).toBeInViewport();
  await expect(newest.getByText("Answer 30.")).toBeInViewport();
  await expect(newest.getByText("Paragraph 12 of a long answer")).not.toBeInViewport();
});

test("a person reading inside a tall newest turn stays there when another turn arrives", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "long");
  await open(page, "Message 1");
  const newest = turns(page).filter({ has: page.getByText("Message 30", { exact: true }) });
  const reading = newest.getByText("Paragraph 6 of a long answer");
  await reading.hover();
  await page.mouse.wheel(0, 190);
  await expect(newest.getByText("Message 30", { exact: true })).not.toBeInViewport();
  await expect(newest.getByText("Paragraph 12 of a long answer")).not.toBeInViewport();
  await fake.step("a first turn arrives");
  await expect(page.getByText("First arrival.")).toBeAttached();
  await page.evaluate(
    "new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)))",
  );
  await expect(reading).toBeInViewport();
  await expect(page.getByText("First arrival.")).not.toBeInViewport();
});

test("a person who read to the end of a tall newest turn is shown the turn that arrives after it", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "long");
  await open(page, "Message 1");
  const newest = turns(page).filter({ has: page.getByText("Message 30", { exact: true }) });
  await newest.getByText("Paragraph 6 of a long answer").hover();
  await page.mouse.wheel(0, 2000);
  await expect(newest.getByText("Paragraph 12 of a long answer")).toBeInViewport();
  await fake.step("a first turn arrives");
  await expect(page.getByText("First arrival.")).toBeInViewport();
});

test("a picture in a person's own message opens in a new tab, and is not downloaded", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "pictures");
  await open(page, "Chart last week's numbers.");
  const sketch = turns(page).first().getByRole("link", { name: "sketch.png" });
  await expect(sketch).toContainText("70 B");
  await expect(sketch).toHaveAttribute("target", "_blank");
  await expect(sketch).not.toHaveAttribute("download");
  await expect(turns(page).first().getByRole("img", { name: "sketch.png" })).toHaveCount(0);
});

test("earlier turns are added above what a person is reading, which stays where it is", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "earlier");
  await open(page, "Message 1");
  await expect(turns(page)).toHaveCount(50);
  const earlier = page.getByRole("button", { name: "Show earlier turns" });
  const reading = turns(page).getByText("Message 71", { exact: true });
  await earlier.scrollIntoViewIfNeeded();
  await expect(reading).toBeInViewport();
  const before = await reading.boundingBox();

  await earlier.click();
  await expect(turns(page)).toHaveCount(100);
  await expect(turns(page).first()).toContainText("Message 21");
  await expect(reading).toBeInViewport();
  // To the nearest pixel: a browser rounds where it scrolls to.
  await expect.poll(async () => (await reading.boundingBox())?.y).toBeCloseTo(before?.y ?? 0, 0);
});

test("a picture stays on the page while its link is renewed at the day's end", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "pictures");
  await open(page, "Chart last week's numbers.");
  const picture = turns(page).first().getByRole("img", { name: "chart.png" });
  await expect(picture).toHaveJSProperty("naturalWidth", 1);
  const link = await picture.getAttribute("src");
  await page.evaluate(
    `document.querySelector('img[alt="chart.png"]').setAttribute("data-shown-before", "yes")`,
  );
  await expect(picture).toHaveAttribute("data-shown-before", "yes");

  await fake.step("the day ends, and the provider ends the stream");
  await expect(picture).not.toHaveAttribute("src", link ?? "");
  await expect(picture).toHaveAttribute("src", /exp=1794960000/u);
  await expect(picture).toHaveAttribute("data-shown-before", "yes");
  await expect(picture).toHaveJSProperty("naturalWidth", 1);
});
