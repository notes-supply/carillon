import type { Page } from "@playwright/test";
import {
  PASSWORD,
  atWork,
  clockAt,
  conversation,
  expect,
  message,
  rail,
  test,
  turns,
  type Fake,
} from "./fixtures.ts";

test.use({ viewport: { width: 390, height: 800 } });

/** Signs in on a window too narrow for the list and the room side by side. */
async function arrive(page: Page, fake: Fake, scenario: string): Promise<void> {
  await fake.play(scenario);
  await clockAt(page, 0);
  await page.goto(fake.origin);
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(message(page)).toBeVisible();
}

function back(page: Page) {
  return page.getByRole("button", { name: "Conversations" });
}

test("on a phone the room shows alone, and Conversations goes back to the list", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "rail");
  await expect(rail(page)).toBeHidden();

  await back(page).click();
  await expect(rail(page)).toBeVisible();
  await expect(message(page)).toBeHidden();

  await conversation(page, "Plan the release.").getByRole("link").click();
  await expect(page.getByRole("heading", { name: "Plan the release.", level: 2 })).toBeVisible();
  await expect(turns(page)).toHaveCount(1);
  await expect(rail(page)).toBeHidden();
});

test("on a phone the list leads back to the conversation already open, and to a new one", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "rail");
  await back(page).click();
  await conversation(page, "Plan the release.").getByRole("link").click();
  await expect(turns(page)).toHaveCount(1);

  await back(page).click();
  await expect(rail(page)).toBeVisible();
  await conversation(page, "Plan the release.").getByRole("link").click();
  await expect(turns(page)).toHaveCount(1);
  await expect(rail(page)).toBeHidden();

  await back(page).click();
  await page.getByRole("link", { name: "New conversation" }).click();
  await expect(page.getByRole("heading", { name: "New conversation", level: 2 })).toBeVisible();
  await expect(rail(page)).toBeHidden();
});

test("on a phone a pressed control leaves the keyboard down", async ({ page, fake }) => {
  await arrive(page, fake, "controls");
  await back(page).click();
  await conversation(page, "Write the long report.").getByRole("link").click();
  await expect(turns(page)).toHaveCount(2);
  const answering = atWork(turns(page).nth(0));
  await answering.getByRole("button", { name: "Stop" }).click();
  await expect(answering.getByRole("button", { name: "Stopping" })).toBeDisabled();
  await expect(message(page)).not.toBeFocused();
});
