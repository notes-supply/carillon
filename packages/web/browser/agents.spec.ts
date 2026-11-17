import { attached, choose } from "./composing.ts";
import { arrive, conversation, expect, message, test } from "./fixtures.ts";

const AUDIENCE = "[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}";

test("a new conversation offers the agents, the first one chosen, and the one picked gets its first message", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "agents");
  const picker = page.getByRole("combobox", { name: "Agent for this conversation" });
  await expect(picker).toHaveValue("assistant");
  await expect(picker.getByRole("option")).toHaveText(["Carillon", "Research"]);
  await expect(page).toHaveURL(new RegExp(`#assistant/${AUDIENCE}$`, "u"));
  const audience = new URL(page.url()).hash.split("/")[1] ?? "";
  await message(page).fill("Find the paper.");
  await choose(page, [{ name: "paper.pdf", bytes: 10 }]);

  await picker.focus();
  await picker.selectOption("Research");
  await expect(page).toHaveURL(new RegExp(`#researcher/${audience}$`, "u"));
  await expect(picker).toHaveValue("researcher");
  await expect(picker).toBeFocused();
  await expect(message(page)).toHaveValue("Find the paper.");
  await expect(attached(page)).toHaveText([/paper\.pdf/u]);

  await message(page).press("Enter");
  await expect(page.getByRole("heading", { name: "Find the paper.", level: 2 })).toBeVisible();
  await expect(conversation(page, "Find the paper.").getByRole("link")).toHaveAttribute(
    "href",
    `#researcher/${audience}`,
  );
  await expect(picker).toHaveCount(0);
  await expect(message(page)).toHaveValue("");
});

test("an installation with one agent offers no choice of agent", async ({ page, fake }) => {
  await arrive(page, fake, "first-message");
  await expect(page.getByText("Write below to start this conversation.")).toBeVisible();
  await expect(page.getByRole("combobox", { name: "Agent for this conversation" })).toHaveCount(0);
});
