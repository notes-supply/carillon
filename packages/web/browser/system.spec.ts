import { arrive, atWork, conversation, expect, open, test, turns } from "./fixtures.ts";

test.describe("a browser without JavaScript", () => {
  test.use({ javaScriptEnabled: false });

  test("is told that the chat needs it", async ({ page, fake }) => {
    await page.goto(fake.origin);
    // Playwright leaves `noscript` out of what it matches as text, so the element is named.
    const said = page.locator("noscript");
    await expect(said).toBeVisible();
    expect(await said.innerText()).toBe("Carillon's chat needs JavaScript.");
  });
});

test("a browser with JavaScript is not told that the chat needs it", async ({ page, fake }) => {
  await fake.play("door");
  await page.goto(fake.origin);
  await expect(page.getByLabel("Password")).toBeVisible();
  await expect(page.getByText("Carillon's chat needs JavaScript.")).toBeHidden();
});

test("the page is dark when the system is, and light when it is not", async ({ page, fake }) => {
  await fake.play("door");
  await page.emulateMedia({ colorScheme: "dark" });
  await page.goto(fake.origin);
  await expect(page.getByLabel("Password")).toBeVisible();
  await expect(page.locator("body")).toHaveCSS("background-color", "rgb(31, 41, 39)");
  await expect(page.locator("body")).toHaveCSS("color", "rgb(226, 232, 227)");
  await page.emulateMedia({ colorScheme: "light" });
  await expect(page.locator("body")).toHaveCSS("background-color", "rgb(248, 250, 247)");
});

test("the marks of work move, and keep still when the system asks for less motion", async ({
  page,
  fake,
}) => {
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await arrive(page, fake, "rail");
  await open(page, "Chart last week's numbers.");
  const mark = conversation(page, "Chart last week's numbers.")
    .getByRole("img", { name: "At work" })
    .locator("i")
    .first();
  const ring = atWork(turns(page).last())
    .getByRole("img", { name: "2 tool calls" })
    .locator("i")
    .last();
  await expect(mark).toHaveCSS("animation-name", "strike");
  await expect(ring).toHaveCSS("animation-name", "sound");
  await page.emulateMedia({ reducedMotion: "reduce" });
  await expect(mark).toHaveCSS("animation-name", "none");
  await expect(ring).toHaveCSS("animation-name", "none");
});
