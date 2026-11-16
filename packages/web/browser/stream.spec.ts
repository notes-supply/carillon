import { arrive, atWork, clockAt, expect, open, test, turns } from "./fixtures.ts";

const AWAY = "Carillon is not answering. Trying again.";

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
  await arrive(page, fake, "unreadable");
  await open(page, "Plan the release.");
  await fake.step("the provider sends what the page cannot read");
  await expect(page.getByText("This page could not read what Carillon answered.")).toBeVisible();
  await expect(turns(page).first()).toContainText("I will start with the notes.");
  await fake.step("the runtime answers a turn of its own");
  await expect(turns(page).last()).toContainText("The release is on schedule.");
  await expect(page.getByText("This page could not read what Carillon answered.")).toBeHidden();
});
