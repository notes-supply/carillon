import { EPOCH, PASSWORD, arrive, clockAt, expect, rail, test } from "./fixtures.ts";
import { streams, tab } from "./tab.ts";

test.beforeEach(async ({ page, fake }) => {
  await fake.play("door");
  await page.clock.setFixedTime(new Date(EPOCH * 1000));
  await page.goto(fake.origin);
});

test("the door says why it is there and waits for the password", async ({ page }) => {
  await expect(page.getByRole("status")).toContainText("Sign in with the web chat's password.");
  await expect(page.getByRole("status").locator("code")).toHaveText("carillon web password");
  await expect(page.getByLabel("Password")).toBeFocused();
  await expect(rail(page)).toBeHidden();
});

test("the form is one a password manager can fill and save", async ({ page }) => {
  const password = page.getByLabel("Password");
  await expect(password).toHaveAttribute("autocomplete", "current-password");
  await expect(password).toHaveAttribute("name", "password");
  await expect(page.locator('input[autocomplete="username"]')).toHaveValue("carillon");
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

test("signing in loads the page afresh", async ({ page }) => {
  const loaded = page.waitForEvent("load", { timeout: 10_000 });
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await loaded;
  await expect(rail(page)).toBeVisible();
});

test("signing in opens the chat in another tab that waited at the door", async ({
  page,
  context,
  fake,
}) => {
  const other = await context.newPage();
  await other.goto(fake.origin);
  await expect(other.getByLabel("Password")).toBeVisible();
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(rail(page)).toBeVisible();
  await expect(rail(other)).toBeVisible();
  await expect(other.getByLabel("Password")).toBeHidden();
});

test("a tab refused with a token that another tab has since replaced goes on with the new one", async ({
  page,
  context,
  fake,
}) => {
  await arrive(page, fake, "signed-out");
  const other = await context.newPage();
  await other.goto(fake.origin);
  await expect(rail(other)).toBeVisible();
  await streams(fake, 2);
  await tab(other, "hidden");
  await streams(fake, 1);

  await fake.step("the password changes");
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(rail(page)).toBeVisible();

  await tab(other, "visible");
  await streams(fake, 2);
  await expect(rail(other)).toBeVisible();
  await expect(other.getByLabel("Password")).toBeHidden();
  await page.reload();
  await expect(rail(page)).toBeVisible();
  await expect(page.getByLabel("Password")).toBeHidden();
});

test("a browser that cannot store the token still signs in, until the page is loaded again", async ({
  page,
  fake,
}) => {
  await page.addInitScript(`Storage.prototype.setItem = () => {
    throw new DOMException("The quota has been exceeded.", "QuotaExceededError");
  };`);
  await page.goto(fake.origin);
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(rail(page)).toBeVisible();
  await expect(page.getByRole("link", { name: "Plan the release." })).toBeVisible();
  await page.reload();
  await expect(page.getByLabel("Password")).toBeVisible();
});
