import { sends } from "./composing.ts";
import {
  arrive,
  atWork,
  conversation,
  conversations,
  expect,
  message,
  open,
  send,
  test,
  turns,
} from "./fixtures.ts";

const AWAY = "Carillon is not answering. Trying again.";

test("a sent message shows at once as on its way, then as its turn, once", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "send-held");
  await open(page, "Plan the release.");
  await send(page, "Write the release notes.");
  await expect(turns(page)).toHaveCount(2);
  await expect(turns(page).last()).toContainText("Write the release notes.");
  await expect(turns(page).last()).toContainText("Carillon is receiving it.");
  await expect(message(page)).toHaveValue("");
  await expect(page.getByRole("button", { name: "Send" })).toBeDisabled();

  await fake.step("the provider reads the upload");
  await expect(atWork(turns(page).last())).toContainText("Waiting its turn");
  await expect(turns(page)).toHaveCount(2);
  await expect(turns(page).getByText("Write the release notes.")).toHaveCount(1);
  await expect(page.getByText("Carillon is receiving it.")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Send" })).toBeEnabled();
});

test("Enter sends, and Shift+Enter starts a new line", async ({ page, fake }) => {
  await arrive(page, fake, "turn");
  await open(page, "Plan the release.");
  await message(page).pressSequentially("Line one");
  await message(page).press("Shift+Enter");
  await message(page).pressSequentially("Line two");
  await expect(message(page)).toHaveValue("Line one\nLine two");
  await expect(turns(page)).toHaveCount(1);
  await message(page).press("Enter");
  await expect(turns(page)).toHaveCount(2);
  await expect(turns(page).last().getByText("Line one")).toHaveText(/^Line one\nLine two$/u, {
    useInnerText: true,
  });
  await expect(message(page)).toHaveValue("");
});

test("a message of nothing but space is not sent", async ({ page, fake }) => {
  await arrive(page, fake, "turn");
  await open(page, "Plan the release.");
  await send(page, "   ");
  await send(page, "Something to send.");
  await expect(turns(page)).toHaveCount(2);
  await expect(turns(page).last()).toContainText("Something to send.");
});

test("what is being written survives a reload, and a sent message leaves nothing behind", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "turn");
  await open(page, "Plan the release.");
  await message(page).fill("Half a thought");
  await page.reload();
  await expect(page.getByRole("heading", { name: "Plan the release.", level: 2 })).toBeVisible();
  await expect(message(page)).toHaveValue("Half a thought");
  await message(page).press("Enter");
  await expect(atWork(turns(page).last())).toContainText("Waiting its turn");
  await page.reload();
  await expect(turns(page)).toHaveCount(2);
  await expect(message(page)).toHaveValue("");
});

test("a message on its way is in the composer again after a reload, with what was written since, and is not sent again", async ({
  page,
  fake,
}) => {
  const sent = sends(page);
  await arrive(page, fake, "send-held");
  await open(page, "Plan the release.");
  await send(page, "Write the release notes.");
  await expect(turns(page).last()).toContainText("Carillon is receiving it.");
  await message(page).fill("And keep them short.");
  await page.reload();
  await expect(page.getByRole("heading", { name: "Plan the release.", level: 2 })).toBeVisible();
  await expect(message(page)).toHaveValue("Write the release notes.\n\nAnd keep them short.");
  await expect(turns(page)).toHaveCount(1);
  await expect(
    page.getByRole("status").filter({ hasText: "Carillon is receiving it." }),
  ).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Send" })).toBeEnabled();
  expect(sent).toHaveLength(1);
});

test("once a message has arrived, a reload shows only what was written since", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "send-held");
  await open(page, "Plan the release.");
  await send(page, "Write the release notes.");
  await expect(turns(page).last()).toContainText("Carillon is receiving it.");
  await message(page).fill("And keep them short.");
  await fake.step("the provider reads the upload");
  await expect(atWork(turns(page).last())).toContainText("Waiting its turn");
  await expect(message(page)).toHaveValue("And keep them short.");
  await page.reload();
  await expect(turns(page)).toHaveCount(2);
  await expect(message(page)).toHaveValue("And keep them short.");
});

test("when a message sent in another tab clears what is stored, this tab stores what it holds again", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "turn");
  await open(page, "Plan the release.");
  await message(page).fill("Mine, unsent.");
  const other = await page.context().newPage();
  await other.goto(page.url());
  await expect(message(other)).toHaveValue("Mine, unsent.");
  await send(other, "Theirs, sent.");
  await expect(atWork(turns(other).last())).toContainText("Waiting its turn");
  await expect(message(other)).toHaveValue("");
  await expect(async () => {
    await other.reload();
    await expect(message(other)).toHaveValue("Mine, unsent.", { timeout: 2000 });
  }).toPass();
  await page.reload();
  await expect(message(page)).toHaveValue("Mine, unsent.");
});

test("what is being written belongs to its conversation", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Plan the release.");
  await message(page).fill("About the release");
  await open(page, "Chart last week's numbers.");
  await expect(message(page)).toHaveValue("");
  await message(page).fill("About the chart");
  await open(page, "Plan the release.");
  await expect(message(page)).toHaveValue("About the release");
  await open(page, "Chart last week's numbers.");
  await expect(message(page)).toHaveValue("About the chart");
});

test("a snapshot does not disturb what is being written", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Plan the release.");
  await message(page).pressSequentially("Half a");
  await fake.step("the runtime answers a turn of its own in the standup conversation");
  await expect(conversations(page).first()).toContainText("Summarise the standup notes.");
  await expect(message(page)).toBeFocused();
  await page.keyboard.type(" thought");
  await expect(message(page)).toHaveValue("Half a thought");
});

test("a refused message comes back to be changed, with the provider's words", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "send-refused");
  await open(page, "Plan the release.");
  await send(page, "Write the release notes.");
  await expect(page.getByRole("alert")).toHaveText(
    "A file may have at most 1024 MiB, and a message's files 2048 MiB together.",
  );
  await expect(message(page)).toHaveValue("Write the release notes.");
  await expect(turns(page)).toHaveCount(1);
  await expect(page.getByRole("button", { name: "Send" })).toBeEnabled();

  await fake.step("the provider takes messages again");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(atWork(turns(page).last())).toContainText("Waiting its turn");
  await expect(turns(page).getByText("Write the release notes.")).toHaveCount(1);
  await expect(message(page)).toHaveValue("");
  await expect(page.getByText("A file may have at most 1024 MiB")).toHaveCount(0);
});

test("a message core took before it went away is sent again and arrives once", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "send-lost-receipt");
  await open(page, "Plan the release.");
  await send(page, "Write the release notes.");
  await expect(page.getByText(AWAY)).toBeVisible();
  await expect(turns(page).last()).toContainText("Write the release notes.");
  await expect(turns(page).last()).toContainText(
    "Carillon is not answering. This message will be sent when it does.",
  );
  await expect(message(page)).toHaveValue("");

  await fake.step("core returns");
  await expect(atWork(turns(page).last())).toContainText("Waiting its turn");
  await expect(page.getByText(AWAY)).toBeHidden();
  await expect(page.getByRole("button", { name: "Send" })).toBeEnabled();
  await expect(turns(page)).toHaveCount(2);
  await expect(turns(page).getByText("Write the release notes.")).toHaveCount(1);
  await expect(page.getByText("This message will be sent when it does.")).toHaveCount(0);
});

test("a new conversation is recorded by its first message", async ({ page, fake }) => {
  await arrive(page, fake, "first-message");
  await expect(page.getByRole("heading", { name: "New conversation", level: 2 })).toBeVisible();
  await expect(page.getByText("Write below to start this conversation.")).toBeVisible();
  await expect(page).toHaveTitle("Carillon");
  await send(page, "Name the new release.");
  await expect(
    page.getByRole("heading", { name: "Name the new release.", level: 2 }),
  ).toBeVisible();
  await expect(conversations(page)).toHaveCount(1);
  await expect(conversation(page, "Name the new release.").getByRole("link")).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(page).toHaveTitle("Name the new release.");
  await expect(page.getByText("Write below to start this conversation.")).toBeHidden();
  await expect(atWork(turns(page).last())).toContainText("Waiting its turn");
  await page.reload();
  await expect(
    page.getByRole("heading", { name: "Name the new release.", level: 2 }),
  ).toBeVisible();
  await expect(turns(page)).toHaveCount(1);
});

test("New conversation leaves the open one for an empty room", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Plan the release.");
  await message(page).fill("About the release");
  await page.getByRole("link", { name: "New conversation" }).click();
  await expect(page.getByRole("heading", { name: "New conversation", level: 2 })).toBeVisible();
  await expect(turns(page)).toHaveCount(0);
  await expect(message(page)).toHaveValue("");
  await expect(conversations(page).locator('[aria-current="page"]')).toHaveCount(0);
  await expect(page.getByText("Write below to start this conversation.")).toBeVisible();
});

test("the composer grows with what is written, up to two fifths of the window, and shrinks again", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "turn");
  await open(page, "Plan the release.");
  const height = async (): Promise<number> => (await message(page).boundingBox())?.height ?? 0;
  const oneLine = await height();
  expect(oneLine).toBeGreaterThan(0);
  await message(page).fill("One\nTwo\nThree\nFour");
  await expect.poll(height).toBeGreaterThan(oneLine * 3);
  await expect.poll(height).toBeLessThan(oneLine * 5);
  await message(page).fill(Array.from({ length: 80 }, () => "A line.").join("\n"));
  await expect.poll(height).toBe(320);
  await message(page).fill("");
  await expect.poll(height).toBe(oneLine);
});

test("opening a conversation in a wide window puts the cursor in the message", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "rail");
  await expect(message(page)).toBeFocused();
  await open(page, "Plan the release.");
  await expect(message(page)).toBeFocused();
  await open(page, "Chart last week's numbers.");
  await expect(message(page)).toBeFocused();
});

test("opening a conversation in a phone's window leaves the message unfocused", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "rail");
  const address = await conversation(page, "Plan the release.")
    .getByRole("link")
    .getAttribute("href");
  await page.setViewportSize({ width: 390, height: 800 });
  await page.goto(`${fake.origin}/${address ?? ""}`);
  await expect(page.getByRole("heading", { name: "Plan the release.", level: 2 })).toBeVisible();
  await expect(turns(page)).toHaveCount(1);
  await expect(message(page)).not.toBeFocused();
});
