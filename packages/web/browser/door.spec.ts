import { EPOCH, PASSWORD, arrive, clockAt, expect, rail, test } from "./fixtures.ts";

test.beforeEach(async ({ page, fake }) => {
  await fake.play("door");
  await page.clock.setFixedTime(new Date(EPOCH * 1000));
  await page.goto(fake.origin);
});

test("the door says why it is there and waits for the password", async ({ page }) => {
  await expect(page.getByRole("status")).toContainText("Sign in with the web chat's password.");
  await expect(page.getByLabel("Password")).toBeFocused();
  await expect(rail(page)).toBeHidden();
});

test("a wrong password is refused in the provider's words, and the field is emptied and focused", async ({
  page,
}) => {
  await page.getByLabel("Password").fill("not the password");
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("alert")).toHaveText("That is not the web chat's password.");
  await expect(page.getByLabel("Password")).toHaveValue("");
  await expect(page.getByLabel("Password")).toBeFocused();
  await expect(rail(page)).toBeHidden();
});

test("the right password opens the chat", async ({ page }) => {
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByLabel("Password").press("Enter");
  await expect(rail(page)).toBeVisible();
  await expect(page.getByLabel("Password")).toBeHidden();
});

test("a reload stays signed in", async ({ page }) => {
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(rail(page)).toBeVisible();
  await page.reload();
  await expect(rail(page)).toBeVisible();
  await expect(page.getByLabel("Password")).toBeHidden();
});

test("when the password changes the door returns, a reload stays at it, and the new sign-in works", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "signed-out");
  await fake.step("the password changes");
  await expect(page.getByRole("status")).toContainText("Sign in with the web chat's password.");
  await expect(rail(page)).toBeHidden();
  await page.reload();
  await expect(page.getByLabel("Password")).toBeVisible();
  await expect(rail(page)).toBeHidden();
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(rail(page)).toBeVisible();
});

test("a page opened while Carillon is away says so, and shows the chat when it returns", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "unavailable");
  await fake.step("core goes away");
  await page.reload();
  await expect(page.getByText("Carillon is not answering. Trying again.")).toBeVisible();
  await expect(page.getByLabel("Password")).toBeHidden();
  await clockAt(page, 9);
  await fake.step("core returns, and the turn it held did not finish");
  await expect(rail(page)).toBeVisible();
  await expect(page.getByText("Carillon is not answering. Trying again.")).toBeHidden();
});
