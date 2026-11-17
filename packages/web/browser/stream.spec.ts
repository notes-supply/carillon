import type { Page } from "@playwright/test";
import {
  EPOCH,
  PASSWORD,
  arrive,
  atWork,
  clockAt,
  expect,
  open,
  rail,
  test,
  turns,
  type Fake,
} from "./fixtures.ts";
import { streams, streamsHeld, tab } from "./tab.ts";

const AWAY = "Carillon is not answering. Trying again.";

async function arriveWithClockHeld(page: Page, fake: Fake, scenario: string): Promise<void> {
  await fake.play(scenario);
  await page.clock.install({ time: new Date(EPOCH * 1000) });
  await page.clock.pauseAt(new Date((EPOCH + 1) * 1000));
  await page.goto(fake.origin);
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(rail(page)).toBeVisible();
}

test("while Carillon is away the page says so and keeps what it shows, then goes on", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "unavailable");
  await open(page, "Plan the release.");
  await expect(atWork(turns(page).last())).toContainText("Drafting.");
  await fake.step("core goes away");
  await expect(page.getByText(AWAY)).toBeVisible();
  await expect(turns(page)).toHaveCount(2);
  await expect(atWork(turns(page).last())).toContainText("Drafting.");

  await clockAt(page, 9);
  await fake.step("core returns, and the turn it held did not finish");
  await expect(turns(page).last()).toContainText("Did not finish 12:00, 39 s");
  await expect(page.getByText(AWAY)).toBeHidden();
});

test("a stream the provider ends is opened again, and what changed since is shown", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "stream-ends");
  await open(page, "Plan the release.");
  await fake.step("the provider ends the stream");
  await fake.step("the runtime answers a turn of its own");
  await expect(turns(page).last()).toContainText("The release is on schedule.");
  await expect(turns(page)).toHaveCount(2);
  await expect(page.getByText(AWAY)).toBeHidden();
});

test("a message the page cannot read is said so, and the next snapshot is shown", async ({
  page,
  fake,
}) => {
  await arriveWithClockHeld(page, fake, "unreadable");
  await open(page, "Plan the release.");
  await expect(turns(page)).toHaveCount(1);
  await fake.step("the provider sends what the page cannot read");
  await expect(page.getByText("This page could not read what Carillon answered.")).toBeVisible();
  await expect(turns(page).first()).toContainText("I will start with the notes.");
  await page.clock.runFor(1000);
  await fake.step("the runtime answers a turn of its own");
  await expect(turns(page).last()).toContainText("The release is on schedule.");
  await expect(page.getByText("This page could not read what Carillon answered.")).toBeHidden();
});

test("a hidden tab leaves the stream, and shows what changed since when it is shown again", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "rail");
  await open(page, "Summarise the standup notes.");
  await expect(turns(page)).toHaveCount(1);
  await streams(fake, 1);

  await tab(page, "hidden");
  await streams(fake, 0);
  await fake.step("the runtime answers a turn of its own in the standup conversation");
  await expect(turns(page)).toHaveCount(1);
  await expect(page.getByText(AWAY)).toBeHidden();

  await tab(page, "visible");
  await expect(turns(page)).toHaveCount(2);
  await expect(page.getByText("Two are unblocked now.")).toBeVisible();
  await streams(fake, 1);
});

test("a stream silent for thirty seconds is given up and opened again, and what changed since is shown", async ({
  page,
  fake,
}) => {
  await arriveWithClockHeld(page, fake, "stream-silent");
  await open(page, "Plan the release.");
  await expect(turns(page)).toHaveCount(1);
  await streams(fake, 1);
  await fake.step("the provider's stream goes silent");
  await fake.step("the runtime answers a turn of its own");

  await page.clock.runFor(29_000);
  expect(await streamsHeld(page)).toBe(0);
  await expect(turns(page)).toHaveCount(1);

  await page.clock.runFor(1000);
  await expect(turns(page)).toHaveCount(2);
  await expect(turns(page).last()).toContainText("The release is on schedule.");
  await expect(page.getByText(AWAY)).toBeHidden();
  await streams(fake, 1);
});
