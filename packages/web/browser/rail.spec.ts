import { audience } from "../fake/world.ts";
import {
  PASSWORD,
  arrive,
  clockAt,
  conversation,
  conversations,
  expect,
  open,
  rail,
  test,
} from "./fixtures.ts";

test("an installation with no conversation says so", async ({ page, fake }) => {
  await arrive(page, fake, "empty");
  await expect(page.getByText("No conversations yet.")).toBeVisible();
  await expect(conversations(page)).toHaveCount(0);
});

test("conversations are listed latest first, and the one at work is marked", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "rail");
  await expect(conversations(page)).toContainText([
    "Chart last week's numbers.",
    "Plan the release.",
    "Files only",
    "Summarise the standup notes.",
  ]);
  await expect(conversations(page).getByRole("img", { name: "At work" })).toHaveCount(1);
  await expect(
    conversation(page, "Chart last week's numbers.").getByRole("img", { name: "At work" }),
  ).toBeVisible();
  await expect(page.getByText("No conversations yet.")).toBeHidden();
});

test("each conversation says when it was last active: the time today, the date before", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "rail");
  await expect(conversation(page, "Chart last week's numbers.")).toContainText("11:55");
  await expect(conversation(page, "Plan the release.")).toContainText("11:40");
  await expect(conversation(page, "Files only")).toContainText("14 Nov");
  await expect(conversation(page, "Summarise the standup notes.")).toContainText("12 Nov");
});

test("the open conversation is the current one in the list", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Plan the release.");
  await expect(conversations(page).locator('[aria-current="page"]')).toHaveCount(1);
  await expect(conversation(page, "Plan the release.").getByRole("link")).toHaveAttribute(
    "aria-current",
    "page",
  );
  await open(page, "Chart last week's numbers.");
  await expect(conversation(page, "Chart last week's numbers.").getByRole("link")).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(conversations(page).locator('[aria-current="page"]')).toHaveCount(1);
});

test("a conversation that changes while it is not open is marked new until it is opened", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "rail");
  await open(page, "Plan the release.");
  await expect(conversations(page).getByRole("img", { name: "New" })).toHaveCount(0);
  await fake.step("the runtime answers a turn of its own in the standup conversation");
  await expect(conversations(page).first()).toContainText("Summarise the standup notes.");
  await expect(
    conversation(page, "Summarise the standup notes.").getByRole("img", { name: "New" }),
  ).toBeVisible();
  await expect(conversations(page).getByRole("img", { name: "New" })).toHaveCount(1);
  await open(page, "Summarise the standup notes.");
  await expect(conversations(page).getByRole("img", { name: "New" })).toHaveCount(0);
  await open(page, "Plan the release.");
  await expect(page.getByText("I will start with the notes.")).toBeVisible();
  await expect(conversations(page).getByRole("img", { name: "New" })).toHaveCount(0);
});

test("a conversation that changes while it is open is not marked new", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Summarise the standup notes.");
  await fake.step("the runtime answers a turn of its own in the standup conversation");
  await expect(page.getByText("Two are unblocked now.")).toBeVisible();
  await open(page, "Plan the release.");
  await expect(page.getByText("I will start with the notes.")).toBeVisible();
  await expect(conversations(page).getByRole("img", { name: "New" })).toHaveCount(0);
});

test("the mark of a conversation at work goes when its turn ends", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Plan the release.");
  await fake.step("the runtime answers a turn of its own in the standup conversation");
  await fake.step("the chart conversation's turn is answered");
  await expect(conversations(page).first()).toContainText("Chart last week's numbers.");
  await expect(conversations(page).getByRole("img", { name: "At work" })).toHaveCount(0);
  await expect(
    conversation(page, "Chart last week's numbers.").getByRole("img", { name: "New" }),
  ).toBeVisible();
});

test("earlier conversations are listed on request, and the keyboard goes to the first of them", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "crowded");
  const earlier = page.getByRole("button", { name: "Show earlier conversations" });
  await expect(conversations(page)).toHaveCount(50);
  await expect(conversations(page).first()).toContainText("Conversation 60");
  await expect(conversations(page).last()).toContainText("Conversation 11");
  await earlier.click();
  await expect(conversations(page)).toHaveCount(60);
  await expect(conversations(page).last()).toContainText("Conversation 1");
  await expect(conversations(page).nth(50).getByRole("link")).toBeFocused();
  await expect(conversations(page).nth(50)).toContainText("Conversation 10");
  await expect(earlier).toBeHidden();
});

test("an installation with one window of conversations offers no earlier ones", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "rail");
  await expect(conversations(page)).toHaveCount(4);
  await expect(page.getByRole("button", { name: "Show earlier conversations" })).toBeHidden();
});

test("a conversation opened by its address is scrolled into view in the list", async ({
  page,
  fake,
}) => {
  await fake.play("crowded");
  await clockAt(page, 0);
  await page.goto(`${fake.origin}/#assistant/${audience(11)}`);
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(rail(page)).toBeVisible();
  await expect(page.getByRole("heading", { name: "Conversation 11", level: 2 })).toBeVisible();
  const row = conversation(page, "Conversation 11").getByRole("link");
  await expect(row).toHaveAttribute("aria-current", "page");
  await expect(row).toBeInViewport();
});

test("a conversation's time becomes its date when the day changes, with no snapshot", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "rail");
  await expect(conversation(page, "Plan the release.")).toContainText("11:40");
  await clockAt(page, 13 * 3600);
  await expect(conversation(page, "Plan the release.")).toContainText("15 Nov");
  await expect(conversation(page, "Chart last week's numbers.")).toContainText("15 Nov");
});

test("beside the room, the list needs no way back", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Plan the release.");
  await expect(rail(page)).toBeVisible();
  await expect(page.getByRole("button", { name: "Conversations" })).toBeHidden();
});
