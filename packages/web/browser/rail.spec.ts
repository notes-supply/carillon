import { arrive, conversation, conversations, expect, open, test } from "./fixtures.ts";

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
