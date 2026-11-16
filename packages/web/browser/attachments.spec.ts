import { attached, choose, drop, paste, sends } from "./composing.ts";
import { arrive, atWork, expect, message, open, test, turns } from "./fixtures.ts";

test("files chosen with the button wait as chips with their sizes, and go with the message", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "turn");
  await open(page, "Plan the release.");
  await choose(page, [
    { name: "notes.txt", bytes: 1500 },
    { name: "figures.csv", bytes: 12 },
  ]);
  await expect(attached(page)).toHaveText([/notes\.txt\s*1\.5 kB/u, /figures\.csv\s*12 B/u]);
  await message(page).fill("Read these.");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(atWork(turns(page).last())).toContainText("Waiting its turn");
  await expect(turns(page).last()).toContainText("Read these.");
  await expect(turns(page).last().getByRole("link")).toHaveText([
    /notes\.txt\s*1\.5 kB/u,
    /figures\.csv\s*12 B/u,
  ]);
  await expect(attached(page)).toHaveCount(0);
});

test("a pasted file and a dropped file are attached beside a chosen one", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "turn");
  await open(page, "Plan the release.");
  await choose(page, [{ name: "chosen.txt", bytes: 3 }]);
  await message(page).fill("Half a thought");
  await paste(page, [{ name: "pasted.png", bytes: 2400 }]);
  await drop(page, [{ name: "dropped.pdf", bytes: 51_000 }]);
  await expect(attached(page)).toHaveText([
    /chosen\.txt\s*3 B/u,
    /pasted\.png\s*2\.4 kB/u,
    /dropped\.pdf\s*51 kB/u,
  ]);
  await expect(message(page)).toHaveValue("Half a thought");
});

test("× takes a file out and moves focus to the next file's ×, and after the last to the message", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "turn");
  await open(page, "Plan the release.");
  await choose(page, [
    { name: "one.txt", bytes: 1 },
    { name: "two.txt", bytes: 2 },
    { name: "three.txt", bytes: 3 },
    { name: "four.txt", bytes: 4 },
  ]);
  await page.getByRole("button", { name: "Remove one.txt" }).click();
  await expect(attached(page)).toHaveText([/two\.txt/u, /three\.txt/u, /four\.txt/u]);
  await expect(page.getByRole("button", { name: "Remove two.txt" })).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(page.getByRole("button", { name: "Remove three.txt" })).toBeFocused();
  await page.getByRole("button", { name: "Remove four.txt" }).click();
  await expect(message(page)).toBeFocused();
  await expect(attached(page)).toHaveText([/three\.txt/u]);
  await page.getByRole("button", { name: "Remove three.txt" }).click();
  await expect(message(page)).toBeFocused();
  await expect(attached(page)).toHaveCount(0);
});

test("files wait with their conversation while another is open", async ({ page, fake }) => {
  await arrive(page, fake, "rail");
  await open(page, "Plan the release.");
  await choose(page, [{ name: "release.txt", bytes: 10 }]);
  await open(page, "Chart last week's numbers.");
  await expect(attached(page)).toHaveCount(0);
  await choose(page, [{ name: "numbers.csv", bytes: 20 }]);
  await open(page, "Plan the release.");
  await expect(attached(page)).toHaveText([/release\.txt/u]);
  await open(page, "Chart last week's numbers.");
  await expect(attached(page)).toHaveText([/numbers\.csv/u]);
});

test("a message of files only can be sent, and its conversation is named Files only", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "first-message");
  await choose(page, [{ name: "scan.pdf", bytes: 7000 }]);
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.getByRole("heading", { name: "Files only", level: 2 })).toBeVisible();
  await expect(turns(page)).toHaveCount(1);
  await expect(turns(page).last().getByRole("link")).toHaveText([/scan\.pdf\s*7 kB/u]);
  await expect(attached(page)).toHaveCount(0);
});

test("more files than a message takes are refused when attached, and Send waits until one is removed", async ({
  page,
  fake,
}) => {
  const sent = sends(page);
  await arrive(page, fake, "limits");
  await open(page, "Plan the release.");
  await choose(page, [
    { name: "one.txt", bytes: 1 },
    { name: "two.txt", bytes: 2 },
    { name: "three.txt", bytes: 3 },
    { name: "four.txt", bytes: 4 },
    { name: "five.txt", bytes: 5 },
  ]);
  await expect(page.getByRole("alert")).toHaveText("A message takes at most 3 files. Remove 2.");
  await page.getByRole("button", { name: "Remove five.txt" }).click();
  await expect(page.getByRole("alert")).toHaveText("A message takes at most 3 files. Remove 1.");
  await message(page).fill("Too many.");
  await message(page).press("Enter");
  await expect(page.getByRole("alert")).toHaveText("A message takes at most 3 files. Remove 1.");
  await expect(message(page)).toHaveValue("Too many.");
  await expect(attached(page)).toHaveCount(4);

  await page.getByRole("button", { name: "Remove one.txt" }).click();
  await expect(page.getByRole("alert")).toBeHidden();
  await page.getByRole("button", { name: "Send" }).click();
  await expect(turns(page)).toHaveCount(2);
  await expect(turns(page).last().getByRole("link")).toHaveCount(3);
  expect(sent).toHaveLength(1);
});

test("a file too large, and files too large together, are refused with their sizes", async ({
  page,
  fake,
}) => {
  const sent = sends(page);
  await arrive(page, fake, "limits");
  await open(page, "Plan the release.");
  await choose(page, [
    { name: "small.txt", bytes: 900 },
    { name: "film.mov", bytes: 2500 },
  ]);
  await expect(page.getByRole("alert")).toHaveText(
    "film.mov is 2.5 kB. A file can be at most 2 kB.",
  );
  await page.getByRole("button", { name: "Remove film.mov" }).click();
  await expect(page.getByRole("alert")).toBeHidden();

  await drop(page, [
    { name: "first.bin", bytes: 1200 },
    { name: "second.bin", bytes: 1100 },
  ]);
  await expect(page.getByRole("alert")).toHaveText(
    "These files are 3.2 kB together. A message takes at most 3 kB.",
  );
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.getByRole("alert")).toHaveText(
    "These files are 3.2 kB together. A message takes at most 3 kB.",
  );
  await page.getByRole("button", { name: "Remove second.bin" }).click();
  await page.getByRole("button", { name: "Send" }).click();
  await expect(turns(page)).toHaveCount(2);
  await expect(turns(page).last().getByRole("link")).toHaveText([
    /small\.txt\s*900 B/u,
    /first\.bin\s*1\.2 kB/u,
  ]);
  expect(sent).toHaveLength(1);
});

test("a file on its way says how much of it has left the browser, beside the message it goes with", async ({
  page,
  fake,
}) => {
  await arrive(page, fake, "send-held");
  await open(page, "Plan the release.");
  await choose(page, [{ name: "film.mov", bytes: 40_000_000 }]);
  await message(page).fill("Watch this.");
  await page.getByRole("button", { name: "Send" }).click();
  const waiting = turns(page).last();
  await expect(waiting).toContainText("Watch this.");
  await expect(waiting.getByRole("listitem")).toHaveText([/film\.mov\s*40 MB/u]);
  await expect(waiting.getByRole("status")).toHaveText(/^Sending, \d{1,2}%\s*$/u);
  await expect(waiting.getByRole("progressbar")).toBeVisible();

  await fake.step("the provider reads the upload");
  await expect(atWork(turns(page).last())).toContainText("Waiting its turn");
  await expect(turns(page).last().getByRole("link")).toHaveText([/film\.mov\s*40 MB/u]);
  await expect(page.getByRole("progressbar")).toHaveCount(0);
  await expect(turns(page)).toHaveCount(2);
});
