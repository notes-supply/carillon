import {
  arrive,
  conversation,
  expect,
  message,
  open,
  rail,
  send,
  test,
  turns,
} from "./fixtures.ts";

const CUSTOM = "This page is your own copy, served from the web provider's site folder";

test("the header names the conversation's agent when Carillon has several", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "header");
  await open(page, "Scout the changelog.");
  await expect(page.getByText("with Scout", { exact: true })).toBeVisible();
  await open(page, "Plan the release.");
  await expect(page.getByText("with Carillon", { exact: true })).toBeVisible();
  await expect(page.getByText("with Scout", { exact: true })).toHaveCount(0);
});

test("the header names no agent when Carillon has one", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Plan the release.");
  await expect(turns(page)).toHaveCount(1);
  await expect(page.getByText(/^with /u)).toHaveCount(0);
});

test("the header names no agent for a conversation that is not recorded yet", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "header");
  await expect(page.getByRole("heading", { name: "New conversation", level: 2 })).toBeVisible();
  await expect(message(page)).toBeVisible();
  await expect(page.getByText(/^with /u)).toHaveCount(0);
});

test("a page served from the owner's own folder says so under the conversations", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "header");
  await expect(rail(page).getByText(CUSTOM)).toBeVisible();
  await expect(rail(page).getByText(CUSTOM).locator("code")).toHaveText("site");
});

test("the release's own page has no footnote", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Plan the release.");
  await expect(turns(page)).toHaveCount(1);
  await expect(page.getByText(CUSTOM)).toHaveCount(0);
});

test("why a message was refused stays with its conversation while another is open", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "send-refused");
  await open(page, "Plan the release.");
  await send(page, "Write the release notes.");
  const refusal = page.getByRole("alert").filter({ hasText: "A file may have at most 1024 MiB" });
  await expect(refusal).toBeVisible();

  await page.getByRole("link", { name: "New conversation" }).click();
  await expect(page.getByRole("heading", { name: "New conversation", level: 2 })).toBeVisible();
  await expect(refusal).toBeHidden();
  await expect(message(page)).toHaveValue("");

  await conversation(page, "Plan the release.").getByRole("link").click();
  await expect(turns(page)).toHaveCount(1);
  await expect(refusal).toBeVisible();
  await expect(message(page)).toHaveValue("Write the release notes.");
});
