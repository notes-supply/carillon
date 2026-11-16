import { commandReads, commands } from "./composing.ts";
import { arrive, expect, message, open, send, test, turns } from "./fixtures.ts";

const MARK = { name: "Command", exact: true } as const;

test("typing / lists the agent's commands, asked for once, and more letters narrow them", async ({
  page,
  fake,
}) => {
  const reads = commandReads(page);
  await arrive(page, fake, "commands");
  await open(page, "Plan the release.");
  await message(page).pressSequentially("No command");
  await message(page).fill("");
  await message(page).pressSequentially("/");
  await expect(commands(page)).toHaveText([
    "/status Report Carillon's status.",
    "/skill:carillon-status Report a job's status. [job]",
    "/compact",
  ]);
  await message(page).pressSequentially("s");
  await expect(commands(page)).toHaveText([/^\/status /u, /^\/skill:carillon-status /u]);
  await message(page).pressSequentially("t");
  await expect(commands(page)).toHaveText([/^\/status /u]);
  await message(page).pressSequentially("x");
  await expect(commands(page)).toHaveCount(0);
  await message(page).press("Backspace");
  await expect(commands(page)).toHaveText([/^\/status /u]);
  expect(reads).toHaveLength(1);
});

test("the arrows move through the commands and wrap, and Tab or Enter writes the chosen one", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "commands");
  await open(page, "Plan the release.");
  const chosen = page.getByRole("option", { selected: true });
  await message(page).pressSequentially("/");
  await expect(chosen).toHaveText(/^\/status /u);
  await message(page).press("ArrowUp");
  await expect(chosen).toHaveText("/compact");
  await message(page).press("ArrowDown");
  await expect(chosen).toHaveText(/^\/status /u);
  await message(page).press("ArrowDown");
  await expect(chosen).toHaveText(/^\/skill:carillon-status /u);
  await message(page).press("Tab");
  await expect(message(page)).toHaveValue("/skill:carillon-status ");
  await expect(message(page)).toBeFocused();
  await expect(commands(page)).toHaveCount(0);

  await message(page).fill("");
  await message(page).pressSequentially("/co");
  await expect(chosen).toHaveText("/compact");
  await message(page).press("Enter");
  await expect(message(page)).toHaveValue("/compact ");
  await expect(turns(page)).toHaveCount(1);
});

test("Escape hides the commands until more is written, and a click writes one and leaves the message focused", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "commands");
  await open(page, "Plan the release.");
  await message(page).pressSequentially("/");
  await expect(commands(page)).toHaveCount(3);
  await message(page).press("Escape");
  await expect(commands(page)).toHaveCount(0);
  await expect(message(page)).toHaveValue("/");
  await message(page).pressSequentially("s");
  await expect(commands(page)).toHaveCount(2);
  await commands(page).filter({ hasText: "/skill:carillon-status" }).click();
  await expect(message(page)).toHaveValue("/skill:carillon-status ");
  await expect(message(page)).toBeFocused();
  await expect(commands(page)).toHaveCount(0);
});

test("a command written in full is sent with Enter, and only its turn is marked Command", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "commands");
  await open(page, "Plan the release.");
  await message(page).pressSequentially("/status");
  await expect(commands(page)).toHaveText([/^\/status /u]);
  await message(page).press("Enter");
  await expect(turns(page)).toHaveCount(2);
  await expect(turns(page).last().getByText(MARK.name, MARK)).toBeVisible();
  await expect(message(page)).toHaveValue("");
  await fake.step("the command is answered");
  await expect(turns(page).last()).toContainText("Carillon is running.");

  await send(page, "/statusquo is no command");
  await expect(turns(page)).toHaveCount(3);
  await expect(turns(page).last()).toContainText("/statusquo is no command");
  await expect(turns(page).getByText(MARK.name, MARK)).toHaveCount(1);
});
