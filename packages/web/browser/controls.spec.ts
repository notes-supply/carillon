import type { Locator, Page } from "@playwright/test";
import {
  arrive,
  atWork,
  clockAt,
  conversation,
  expect,
  message,
  open,
  test,
  turns,
  type Fake,
} from "./fixtures.ts";

const TITLE = "Write the long report.";
const UNANSWERED = "Carillon did not answer. If the turn goes on, press it again.";
const CONTROL = "**/api/messages/*/control";

function answering(page: Page): Locator {
  return atWork(turns(page).nth(0));
}

function waiting(page: Page): Locator {
  return atWork(turns(page).nth(1));
}

async function press(page: Page, turn: Locator, name: string): Promise<void> {
  const answered = page.waitForResponse(CONTROL);
  await turn.getByRole("button", { name }).click();
  await answered;
}

async function begin(page: Page, fake: Fake): Promise<void> {
  await arrive(page, fake, "controls");
  await open(page, TITLE);
  await expect(turns(page)).toHaveCount(2);
}

test("a turn at work offers the controls its runtime offers, and each says what it does", async ({
  page,
  fake,
}) => {
  await begin(page, fake);
  await expect(answering(page).getByRole("button")).toHaveText(["Stop"]);
  await expect(answering(page).getByRole("button", { name: "Stop" })).toHaveAttribute(
    "title",
    "End this answer.",
  );
  await expect(waiting(page).getByRole("button")).toHaveText(["Steer", "Interrupt", "Stop"]);
  await expect(waiting(page).getByRole("button", { name: "Steer" })).toHaveAttribute(
    "title",
    "Hand this message to the answer that is running.",
  );
  await expect(waiting(page).getByRole("button", { name: "Interrupt" })).toHaveAttribute(
    "title",
    "End the running answer and run this message next.",
  );
  await expect(waiting(page).getByRole("button", { name: "Stop" })).toHaveAttribute(
    "title",
    "Do not run this message.",
  );
});

test("a turn that offers no control shows none", async ({ page, fake }) => {
  await arrive(page, fake, "endings");
  await open(page, "Summarise the meeting.");
  await expect(atWork(turns(page).nth(6))).toContainText("At work");
  await expect(atWork(turns(page).nth(6)).getByRole("button")).toHaveCount(0);
  await expect(atWork(turns(page).nth(7)).getByRole("button")).toHaveCount(0);
});

for (const { control, pressed, others, on, after } of [
  {
    control: "Steer",
    pressed: "Steering",
    others: ["Interrupt", "Stop"],
    on: waiting,
    after: "A later message joined this one.",
  },
  {
    control: "Interrupt",
    pressed: "Interrupting",
    others: ["Steer", "Stop"],
    on: waiting,
    after: "Stopped 12:00, 2 min 3 s",
  },
  {
    control: "Stop",
    pressed: "Stopping",
    others: [],
    on: answering,
    after: "Stopped 12:00, 2 min 3 s",
  },
] as const)
  test(`${control} reads ${pressed} and holds the turn's other controls until the runtime acts, and the keyboard returns to the message`, async ({
    page,
    fake,
  }) => {
    await begin(page, fake);
    const turn = on(page);
    await press(page, turn, control);
    await expect(turn.getByRole("button", { name: pressed })).toBeDisabled();
    for (const other of others)
      await expect(turn.getByRole("button", { name: other })).toBeDisabled();
    await expect(turn.getByRole("button")).toHaveCount(others.length + 1);
    await expect(message(page)).toBeFocused();

    await clockAt(page, 5);
    await expect(turn.getByRole("button", { name: pressed })).toBeDisabled();

    await fake.step("the runtime acts on the control");
    await expect(turns(page).nth(0)).toContainText(after);
    await expect(atWork(turns(page).nth(0))).toHaveCount(0);
    await expect(waiting(page).getByRole("button")).toHaveText(["Stop"]);
    await expect(waiting(page).getByRole("button", { name: "Stop" })).toBeEnabled();
    await expect(waiting(page).getByRole("button", { name: "Stop" })).toHaveAttribute(
      "title",
      "End this answer.",
    );
  });

test("Stop on a waiting message reads Stopping until that message is stopped, and leaves the answer its own Stop", async ({
  page,
  fake,
}) => {
  await begin(page, fake);
  await press(page, waiting(page), "Stop");
  await expect(waiting(page).getByRole("button", { name: "Stopping" })).toBeDisabled();
  await expect(waiting(page).getByRole("button", { name: "Steer" })).toBeDisabled();
  await expect(answering(page).getByRole("button", { name: "Stop" })).toBeEnabled();
  await fake.step("the runtime acts on the control");
  await expect(turns(page).nth(1)).toContainText("Stopped 12:00, 23 s");
  await expect(answering(page).getByRole("button")).toHaveText(["Stop"]);
  await expect(answering(page).getByRole("button", { name: "Stop" })).toBeEnabled();
});

test("a control Carillon does not answer says to press it again, and it can be", async ({
  page,
  fake,
}) => {
  await begin(page, fake);
  await page.route(CONTROL, (route) => route.abort());
  await waiting(page).getByRole("button", { name: "Steer" }).click();
  await expect(waiting(page).getByRole("alert")).toHaveText(UNANSWERED);
  await expect(waiting(page).getByRole("button")).toHaveText(["Steer", "Interrupt", "Stop"]);
  for (const button of await waiting(page).getByRole("button").all())
    await expect(button).toBeEnabled();
  await expect(message(page)).toBeFocused();

  await page.unroute(CONTROL);
  await press(page, waiting(page), "Steer");
  await expect(waiting(page).getByRole("button", { name: "Steering" })).toBeDisabled();
  await expect(waiting(page).getByRole("alert")).toHaveCount(0);
  await fake.step("the runtime acts on the control");
  await expect(turns(page).nth(0)).toContainText("A later message joined this one.");
});

test("a control answered while Carillon is away says to press it again", async ({ page, fake }) => {
  await begin(page, fake);
  await page.route(CONTROL, (route) =>
    route.fulfill({
      status: 503,
      contentType: "application/json",
      body: '{"error":"Carillon is not answering right now. Try again in a moment."}',
    }),
  );
  await answering(page).getByRole("button", { name: "Stop" }).click();
  await expect(answering(page).getByRole("alert")).toHaveText(UNANSWERED);
  await expect(answering(page).getByRole("button", { name: "Stop" })).toBeEnabled();
});

test("what a control did, or why it was not taken, stays with its turn while another conversation is open", async ({
  page,
  fake,
}) => {
  await begin(page, fake);
  await page.route(CONTROL, (route) => route.abort());
  await answering(page).getByRole("button", { name: "Stop" }).click();
  await expect(answering(page).getByRole("alert")).toHaveText(UNANSWERED);
  await page.unroute(CONTROL);
  await waiting(page).getByRole("button", { name: "Interrupt" }).click();
  await expect(waiting(page).getByRole("button", { name: "Interrupting" })).toBeDisabled();

  await page.getByRole("link", { name: "New conversation" }).click();
  await expect(page.getByRole("heading", { name: "New conversation", level: 2 })).toBeVisible();
  await expect(turns(page)).toHaveCount(0);
  await conversation(page, TITLE).getByRole("link").click();
  await expect(turns(page)).toHaveCount(2);

  await expect(answering(page).getByRole("alert")).toHaveText(UNANSWERED);
  await expect(waiting(page).getByRole("button", { name: "Interrupting" })).toBeDisabled();
  await expect(waiting(page).getByRole("button", { name: "Steer" })).toBeDisabled();
});

test("a control its runtime still offers fifteen seconds after it was pressed can be pressed again", async ({
  page,
  fake,
}) => {
  await begin(page, fake);
  await answering(page).getByRole("button", { name: "Stop" }).click();
  await expect(answering(page).getByRole("button", { name: "Stopping" })).toBeDisabled();
  await clockAt(page, 15);
  await expect(answering(page).getByRole("button", { name: "Stopping" })).toBeDisabled();
  await clockAt(page, 16);
  await expect(answering(page).getByRole("button", { name: "Stop" })).toBeEnabled();
});
